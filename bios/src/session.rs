// A gamescope that stays up while the bios and games take turns in it, so switching between them
// does not restart gamescope or change the display mode. The bios uses gamescope's first Xwayland
// server and games a second one, resized for each game. Scaling options are applied through
// gamescope's X root window properties, the same way Steam configures gamescope on SteamOS. Only
// games needing options that cannot change while gamescope runs (like the output resolution)
// still get a gamescope of their own.
//
// Games share their Xwayland server, rather than getting a new one each, as gamescope deadlocks
// when asked to create one while running (GAMESCOPE_CREATE_XWAYLAND_SERVER).
//
// The bios stays on screen, showing that the game is starting, until the game shows a window: it
// is kept in front with gamescope's focus control, the way Steam picks the app gamescope shows.

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, PropMode, Window};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::launcher;
use crate::signals;

/// Argument gamescope's primary child is started with; it records gamescope's environment and exits
pub const HOLD_ARG: &str = "--gamescope-hold";

/// How long gamescope gets to start up and run its primary child
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long gamescope gets to answer a request
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long gamescope gets to exit after a termination signal before it is killed
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// gamescope's sharpness when none is given
const DEFAULT_SHARPNESS: u32 = 2;
/// Made-up Steam app id the bios window is given, to tell gamescope to keep showing it
const BIOS_APP_ID: u32 = 0x6b7a;
/// How often gamescope's window list is checked for the game's window
const WINDOW_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn env_file() -> PathBuf {
    launcher::runtime_dir().join("kazeta-gamescope-env")
}

/// Runs as gamescope's primary child: writes the environment gamescope gives its children
/// (DISPLAY and friends), so the launcher can start the bios and games with it
pub fn hold(env_file: &str) -> i32 {
    let mut data = Vec::new();
    for (key, value) in std::env::vars_os() {
        data.extend_from_slice(key.as_bytes());
        data.push(b'=');
        data.extend_from_slice(value.as_bytes());
        data.push(0);
    }
    // written in one go, so the launcher never reads half of it
    let tmp = format!("{}.tmp", env_file);
    if let Err(e) = fs::write(&tmp, data).and_then(|_| fs::rename(&tmp, env_file)) {
        eprintln!("Failed to write {}: {}", env_file, e);
        return 1;
    }
    0
}

fn read_env(data: &[u8]) -> Vec<(OsString, OsString)> {
    data.split(|&b| b == 0)
        .filter_map(|entry| {
            let at = entry.iter().position(|&b| b == b'=')?;
            Some((OsString::from_vec(entry[..at].to_vec()), OsString::from_vec(entry[at + 1..].to_vec())))
        })
        .collect()
}

/// gamescope's scaling filters, by their values for GAMESCOPE_NEW_SCALING_FILTER
const FILTERS: &[&str] = &["linear", "nearest", "fsr", "nis", "pixel", "sgsr"];
/// gamescope's scalers, by their values for GAMESCOPE_NEW_SCALING_SCALER
const SCALERS: &[&str] = &["auto", "integer", "fit", "fill", "stretch"];

/// Gamescope options that can be changed while gamescope runs; unset ones get gamescope's defaults
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RuntimeOptions {
    pub filter: Option<u32>,
    pub scaler: Option<u32>,
    pub sharpness: Option<u32>,
    /// resolution of the game's Xwayland server (-w/-h)
    pub size: Option<(u32, u32)>,
}

/// Split gamescope options into those that can be applied to a running gamescope and the rest,
/// which need gamescope to be started with them. Anything not understood is left in the rest.
pub fn split_options(args: &[String]) -> (RuntimeOptions, Vec<String>) {
    let mut runtime = RuntimeOptions::default();
    let mut startup = Vec::new();
    let (mut width, mut height) = (None, None);

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // --name=value or a separate value
        let (name, inline_value) = match arg.split_once('=') {
            Some((name, value)) if arg.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg, None),
        };
        let known = matches!(
            name,
            "-w" | "--nested-width" | "-h" | "--nested-height" | "-F" | "--filter" | "-S" | "--scaler" | "--sharpness" | "--fsr-sharpness"
        );
        let value = inline_value.clone().or_else(|| args.get(i + 1).cloned());
        let consumed = if inline_value.is_some() { 1 } else { 2 };

        let parsed = known.then_some(()).and(value.as_deref()).and_then(|value| {
            let index_of = |names: &[&str]| names.iter().position(|n| *n == value).map(|i| i as u32);
            match name {
                "-w" | "--nested-width" => value.parse().ok().map(|v| width = Some(v)),
                "-h" | "--nested-height" => value.parse().ok().map(|v| height = Some(v)),
                "-F" | "--filter" => index_of(FILTERS).map(|v| runtime.filter = Some(v)),
                "-S" | "--scaler" => index_of(SCALERS).map(|v| runtime.scaler = Some(v)),
                _ => value.parse::<u32>().ok().map(|v| runtime.sharpness = Some(v.min(20))),
            }
        });

        if parsed.is_some() {
            i += consumed;
        } else {
            startup.push(args[i].clone());
            i += 1;
        }
    }

    match (width, height) {
        (Some(w), Some(h)) => runtime.size = Some((w, h)),
        // gamescope works out the other one from the output, which is left to a gamescope of its own
        (Some(w), None) => startup.extend(["-w".to_string(), w.to_string()]),
        (None, Some(h)) => startup.extend(["-h".to_string(), h.to_string()]),
        (None, None) => {}
    }
    (runtime, startup)
}

/// gamescope's Xwayland server for games
const GAME_SERVER: u32 = 1;

pub struct Session {
    child: Child,
    /// options gamescope was started with that it cannot change while running
    pub startup_args: Vec<String>,
    /// environment for gamescope's clients, with DISPLAY set to its first Xwayland server
    pub env: Vec<(OsString, OsString)>,
    /// DISPLAY of the Xwayland server for games
    pub game_display: String,
}

impl Session {
    /// Start gamescope with `startup_args`, waiting until it is ready for clients
    pub fn start(startup_args: Vec<String>, stop: &dyn Fn() -> bool) -> Option<Session> {
        let env_file = env_file();
        let _ = fs::remove_file(&env_file);

        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kazeta"));
        eprintln!("+ gamescope {:?} --keep-alive --xwayland-count 2 -- {} {} {}", startup_args, exe.display(), HOLD_ARG, env_file.display());
        let child = Command::new("gamescope")
            .args(&startup_args)
            // the primary child only reports the environment, gamescope has to outlive it
            .arg("--keep-alive")
            // a second Xwayland server for games, see above
            .args(["--xwayland-count", "2"])
            .arg("--")
            .arg(exe)
            .arg(HOLD_ARG)
            .arg(&env_file)
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(e) => {
                eprintln!("Failed to start gamescope: {}", e);
                return None;
            }
        };
        // stopped by Drop if it never gets ready
        let mut session = Session { child, startup_args, env: Vec::new(), game_display: String::new() };

        let started = Instant::now();
        loop {
            if let Ok(data) = fs::read(&env_file) {
                session.env = read_env(&data);
                let _ = fs::remove_file(&env_file);
                break;
            }
            if !session.running() {
                eprintln!("gamescope exited while starting");
                return None;
            }
            if stop() || started.elapsed() > START_TIMEOUT {
                eprintln!("gamescope did not get ready");
                return None;
            }
            thread::sleep(POLL_INTERVAL);
        }

        let Some(display) = session.display() else {
            eprintln!("gamescope did not set DISPLAY");
            return None;
        };
        let Some(game_display) = find_game_display(&display) else {
            eprintln!("gamescope's Xwayland server for games was not found");
            return None;
        };
        eprintln!("gamescope is ready, bios on {}, games on {}", display, game_display);
        session.game_display = game_display;
        Some(session)
    }

    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn display(&self) -> Option<String> {
        self.env.iter().find(|(key, _)| key == "DISPLAY").map(|(_, value)| value.to_string_lossy().into_owned())
    }

    /// Connection to gamescope's first Xwayland server, whose root window takes its control properties
    fn control(&self) -> Option<Control> {
        Control::connect(&self.display()?)
    }

    /// Apply `options` to the whole of gamescope, using gamescope's defaults for unset ones
    pub fn apply(&self, options: &RuntimeOptions) {
        if let Some(control) = self.control() {
            control.apply(options);
        }
    }

    /// Keep `bios` on screen while a game starts. Once a window of anything else shows up, the bios
    /// is stopped and the game's `options` applied, so the bios is not drawn with them. Without a
    /// game window, that happens when the returned handoff is dropped.
    pub fn hand_off(&self, bios: RunningBios, options: RuntimeOptions) -> Handoff {
        let done = Arc::new(AtomicBool::new(false));
        let control = self.control();
        let thread = {
            let done = done.clone();
            thread::spawn(move || hand_off(control, bios, options, &done))
        };
        Handoff { done, thread: Some(thread) }
    }

    /// Size the games' Xwayland server for a game, to its -w/-h or else the output's size
    pub fn prepare_game_display(&self, options: &RuntimeOptions) {
        let Some(control) = self.control() else {
            return;
        };
        let (width, height) = options.size.unwrap_or_else(|| {
            let screen = &control.conn.setup().roots[0];
            (u32::from(screen.width_in_pixels), u32::from(screen.height_in_pixels))
        });
        // allowing sizes beyond the output's, like -w/-h do
        control.set("GAMESCOPE_XWAYLAND_MODE_CONTROL", &[GAME_SERVER, width, height, 1]);
        if !wait_for_size(&self.game_display, width, height) {
            eprintln!("Xwayland server {} did not change to {}x{}", self.game_display, width, height);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = signals::wait_child(&mut self.child, &|| true, KILL_TIMEOUT);
    }
}

/// A bios left running after it handed over a cart; stopped when dropped
pub struct RunningBios(pub Child);

impl Drop for RunningBios {
    fn drop(&mut self) {
        let _ = signals::wait_child(&mut self.0, &|| true, KILL_TIMEOUT);
    }
}

/// Switches from the bios to the game, see `Session::hand_off`
pub struct Handoff {
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Handoff {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn hand_off(control: Option<Control>, bios: RunningBios, options: RuntimeOptions, done: &AtomicBool) {
    let Some(control) = control else {
        return;
    };
    let bios_pid = bios.0.id();

    // the bios window is told apart by its process, as both Xwayland servers hand out the same window ids
    let bios_window = control.focusable_windows().into_iter().find(|w| w.pid == bios_pid).map(|w| w.window);
    let pinned = bios_window.is_some_and(|window| {
        control.set_on(window, "STEAM_GAME", &[BIOS_APP_ID]) && control.set("GAMESCOPECTRL_BASELAYER_APPID", &[BIOS_APP_ID])
    });
    if !pinned {
        eprintln!("Could not keep the bios in front, the game may show before its window");
    }

    while !done.load(Ordering::Relaxed) {
        if let Some(window) = control.focusable_windows().into_iter().find(|w| w.pid != bios_pid) {
            eprintln!("Game window {:#x} of process {} showed up", window.window, window.pid);
            break;
        }
        thread::sleep(WINDOW_POLL_INTERVAL);
    }

    // the game takes over once the bios and its focus pin are gone
    drop(bios);
    control.apply(&options);
    if pinned {
        control.delete("GAMESCOPECTRL_BASELAYER_APPID");
    }
}

/// The display of gamescope's Xwayland server for games. gamescope only tells its children about
/// its first server; the second is the next display that says it is server 1.
fn find_game_display(display: &str) -> Option<String> {
    let first: u32 = display.trim_start_matches(':').parse().ok()?;
    let started = Instant::now();
    while started.elapsed() < REQUEST_TIMEOUT {
        for number in first + 1..first + 16 {
            let candidate = format!(":{}", number);
            if !PathBuf::from(format!("/tmp/.X11-unix/X{}", number)).exists() {
                continue;
            }
            let Ok((conn, screen)) = x11rb::connect(Some(&candidate)) else {
                continue;
            };
            let control = Control { root: conn.setup().roots[screen].root, conn };
            if control.get_cardinal("GAMESCOPE_XWAYLAND_SERVER_ID") == Some(GAME_SERVER) {
                return Some(candidate);
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
    None
}

/// Wait until the root window of `display` has the given size
fn wait_for_size(display: &str, width: u32, height: u32) -> bool {
    let Ok((conn, screen)) = x11rb::connect(Some(display)) else {
        return false;
    };
    let root = conn.setup().roots[screen].root;
    let started = Instant::now();
    while started.elapsed() < REQUEST_TIMEOUT {
        if let Ok(Ok(geometry)) = conn.get_geometry(root).map(|cookie| cookie.reply()) {
            if u32::from(geometry.width) == width && u32::from(geometry.height) == height {
                return true;
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
    false
}

/// An entry of GAMESCOPE_FOCUSABLE_WINDOWS, which also has the window's app id
struct FocusableWindow {
    window: u32,
    pid: u32,
}

struct Control {
    conn: RustConnection,
    root: Window,
}

impl Control {
    fn connect(display: &str) -> Option<Control> {
        match x11rb::connect(Some(display)) {
            Ok((conn, screen)) => {
                let root = conn.setup().roots[screen].root;
                Some(Control { conn, root })
            }
            Err(e) => {
                eprintln!("Failed to connect to gamescope on {}: {}", display, e);
                None
            }
        }
    }

    fn atom(&self, name: &str) -> Option<Atom> {
        self.conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok().map(|reply| reply.atom)
    }

    /// Set a CARDINAL property on the root window, which gamescope acts on
    fn set(&self, name: &str, values: &[u32]) -> bool {
        self.set_on(self.root, name, values)
    }

    fn set_on(&self, window: Window, name: &str, values: &[u32]) -> bool {
        let set = self.atom(name).and_then(|atom| {
            self.conn.change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, values).ok()?;
            self.conn.flush().ok()
        });
        if set.is_none() {
            eprintln!("Failed to set {}", name);
        }
        set.is_some()
    }

    fn delete(&self, name: &str) {
        if let Some(atom) = self.atom(name) {
            let _ = self.conn.delete_property(self.root, atom);
            let _ = self.conn.flush();
        }
    }

    fn get_cardinals(&self, name: &str) -> Vec<u32> {
        let values = || {
            let atom = self.atom(name)?;
            let reply = self.conn.get_property(false, self.root, atom, AtomEnum::CARDINAL, 0, 1024).ok()?.reply().ok()?;
            let values: Vec<u32> = reply.value32()?.collect();
            Some(values)
        };
        values().unwrap_or_default()
    }

    fn get_cardinal(&self, name: &str) -> Option<u32> {
        self.get_cardinals(name).first().copied()
    }

    /// Windows of all Xwayland servers that gamescope could show
    fn focusable_windows(&self) -> Vec<FocusableWindow> {
        self.get_cardinals("GAMESCOPE_FOCUSABLE_WINDOWS")
            .chunks_exact(3)
            .map(|entry| FocusableWindow { window: entry[0], pid: entry[2] })
            .collect()
    }

    /// Apply `options` to the whole of gamescope, using gamescope's defaults for unset ones
    fn apply(&self, options: &RuntimeOptions) {
        eprintln!("Applying gamescope options {:?}", options);
        self.set("GAMESCOPE_NEW_SCALING_FILTER", &[options.filter.unwrap_or(0)]);
        self.set("GAMESCOPE_NEW_SCALING_SCALER", &[options.scaler.unwrap_or(0)]);
        self.set("GAMESCOPE_SHARPNESS", &[options.sharpness.unwrap_or(DEFAULT_SHARPNESS)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn splits_runtime_options() {
        let (runtime, startup) = split_options(&args("-w 640 -h 480 -F nearest --scaler=integer --sharpness 5 -W 1920 -H 1080 -r 30"));
        assert_eq!(runtime, RuntimeOptions { filter: Some(1), scaler: Some(1), sharpness: Some(5), size: Some((640, 480)) });
        assert_eq!(startup, args("-W 1920 -H 1080 -r 30"));
    }

    #[test]
    fn keeps_unknown_values_for_startup() {
        let (runtime, startup) = split_options(&args("--filter bogus -w 640"));
        assert_eq!(runtime, RuntimeOptions::default());
        assert_eq!(startup, args("--filter bogus -w 640"));
    }
}

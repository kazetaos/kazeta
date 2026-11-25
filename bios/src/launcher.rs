// Cart launcher: mounts a cart and runs it under gamescope.
// Without a cart argument the bios UI is started first; it waits for a cart and hands it back here,
// and is started again once the game exits. The bios and games share one gamescope session where
// possible, see session.rs. Pressing the power button stops a running game, or
// shuts down when pressed in the bios.
// Session duties (controllers, audio, logging, power off) are left to kazeta-session.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use chrono::{Local, SecondsFormat};

use crate::mount::{self, Slot};
use crate::save;
use crate::power;
use crate::session::{self, RunningBios, RuntimeOptions, Session};
use crate::signals;
use crate::update;
use crate::volume;

const NONE_RUNTIME: &str = "/usr/share/kazeta/runtimes/none.kzr";
/// Argument the launcher starts the bios UI with under gamescope
pub const UI_ARG: &str = "--ui";
const PLAYTIME_INTERVAL: Duration = Duration::from_secs(60);
/// How long gamescope gets to exit after a termination signal before it is killed
const GAMESCOPE_KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-user directory for files handed between processes, so multiple users or instances do not collide
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

fn cart_exec_file() -> PathBuf {
    runtime_dir().join("kazeta-cart-exec")
}

/// The cart's `Env` lines (`NAME=value`), one per line, for the runtime to export
fn cart_env_file() -> PathBuf {
    runtime_dir().join("kazeta-cart-env")
}

/// Written by the bios UI with the carts to launch, one path per line: the base cart, then its
/// DLC bottom to top
pub fn boot_cart_file() -> PathBuf {
    runtime_dir().join("kazeta-boot-cart")
}

fn now_iso8601() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Gamescope options for the resolution limit selected in the display menu
fn resolution_gamescope_args() -> Vec<String> {
    let value = fs::read_to_string(crate::resolution_file_path()).unwrap_or_default();
    let args: &[&str] = match value.trim() {
        "1080p" => &["-W", "1920", "-H", "1080"],
        "720p" => &["-W", "1280", "-H", "720"],
        _ => &[],
    };
    args.iter().map(|s| s.to_string()).collect()
}

/// Gamescope options for the display orientation in the state directory's .orientation file
/// (normal, left, right or upsidedown); empty if the file is missing or empty
fn orientation_gamescope_args() -> Vec<String> {
    let path = save::get_kazeta_data_dir().join("state/.orientation");
    match fs::read_to_string(path) {
        Ok(value) if !value.trim().is_empty() => vec!["--force-orientation".to_string(), value.trim().to_string()],
        _ => Vec::new(),
    }
}

/// Find the first file with the given extension in `dir` or its immediate subdirectories
fn find_cart_file(dir: &Path, extension: &str) -> Option<PathBuf> {
    save::find_files_by_extension(dir, extension, 1, true)
        .ok()
        .and_then(|files| files.into_iter().next())
        .map(PathBuf::from)
}

/// Directory on external media, relative to its root, that holds additional carts
const MEDIA_CONTENT_DIR: &str = "kazeta/content";
/// Directory on external media, relative to its root, that holds additional runtimes
const MEDIA_RUNTIMES_DIR: &str = "kazeta/runtimes";

/// Root directories of all mounted external media
fn media_roots(base_ext: &Path) -> Vec<PathBuf> {
    fs::read_dir(base_ext)
        .map(|entries| entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default()
}

/// The root of the external media `path` is on, if it is on any
fn media_root_of(path: &Path) -> Option<PathBuf> {
    let base_ext = PathBuf::from(save::get_external_media_dir());
    let relative = path.strip_prefix(&base_ext).ok()?;
    let first = relative.components().next()?;
    let root = base_ext.join(first);
    root.is_dir().then_some(root)
}

/// Find all inserted carts, cart packages first: at the root of external media and in its
/// kazeta/content directory, both including their immediate subdirectories
pub fn find_carts() -> Vec<PathBuf> {
    let base_ext = PathBuf::from(save::get_external_media_dir());
    let content_dirs: Vec<PathBuf> = media_roots(&base_ext)
        .into_iter()
        .map(|root| root.join(MEDIA_CONTENT_DIR))
        .filter(|dir| dir.is_dir())
        .collect();
    let mut carts = Vec::new();
    for extension in ["kzp", "kzi"] {
        let mut files = save::find_files_by_extension(&base_ext, extension, 1, false).unwrap_or_default();
        for dir in &content_dirs {
            files.extend(save::find_files_by_extension(dir, extension, 1, false).unwrap_or_default());
        }
        files.sort();
        files.dedup();
        carts.extend(files);
    }
    carts
}

/// What the cart selection needs to know about a cart
#[derive(Clone)]
pub struct CartDetails {
    pub name: String,
    /// contents of the icon file, if the cart has one
    pub icon: Option<Vec<u8>>,
    pub id: String,
    /// for DLC, the id of the base game it extends
    pub extends: Option<String>,
    /// whether the cart has its own Exec; DLC with one is a game of its own
    pub has_exec: bool,
}

/// The id of the base game in an `Extends` value, `<base game id>:<base game hash>`. The hash
/// is only an annotation and is not checked, as hashing the base game would be too slow.
fn extended_id(extends: &str) -> Option<String> {
    let id = extends.split_once(':').map_or(extends, |(id, _)| id).trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// A game that can be started: a base cart and the DLC played with it
pub struct LaunchEntry {
    pub name: String,
    pub icon: Option<Vec<u8>>,
    /// the base cart first, then its DLC bottom to top; DLC with its own Exec, which is what
    /// runs, comes last
    pub carts: Vec<PathBuf>,
}

/// The games that can be started with the inserted carts. Each base game is played with all of
/// its DLC that has no Exec of its own; DLC that has one is a separate game, played with the same
/// DLC as its base game and on top of it. DLC whose base game is not inserted is left out.
pub fn launch_entries(carts: &[(PathBuf, CartDetails)]) -> Vec<LaunchEntry> {
    let mut entries = Vec::new();
    for (base_path, base) in carts.iter().filter(|(_, cart)| cart.extends.is_none()) {
        let dlc: Vec<&(PathBuf, CartDetails)> = carts.iter()
            .filter(|(_, cart)| !base.id.trim().is_empty() && cart.extends.as_deref() == Some(base.id.trim()))
            .collect();
        let add_ons: Vec<PathBuf> = dlc.iter().filter(|(_, cart)| !cart.has_exec).map(|(path, _)| path.clone()).collect();
        let base_carts: Vec<PathBuf> = std::iter::once(base_path.clone()).chain(add_ons).collect();

        entries.push(LaunchEntry { name: base.name.clone(), icon: base.icon.clone(), carts: base_carts.clone() });
        for (path, cart) in dlc.into_iter().filter(|(_, cart)| cart.has_exec) {
            entries.push(LaunchEntry {
                name: cart.name.clone(),
                icon: cart.icon.clone().or_else(|| base.icon.clone()),
                carts: base_carts.iter().cloned().chain([path.clone()]).collect(),
            });
        }
    }
    entries
}

/// Read the name and icon of a cart for display. A cart package is briefly mounted to read
/// its cart info. Falls back to the file name when there is no name.
pub fn cart_details(cart: &Path) -> CartDetails {
    let is_pkg = cart.extension().is_some_and(|ext| ext == "kzp");
    let mut details = if is_pkg {
        // separate from the mount point used to run a cart
        let details = mount::mount_pkg(Slot::Preview, cart)
            .and_then(|preview_mount| find_cart_file(&preview_mount, "kzi"))
            .map(|info| read_cart_details(&info));
        mount::unmount(&[Slot::Preview]);
        details
    } else {
        Some(read_cart_details(cart))
    }
    .unwrap_or(CartDetails { name: String::new(), icon: None, id: String::new(), extends: None, has_exec: false });

    if details.name.is_empty() {
        details.name = cart.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    }
    details
}

/// Copy cart info and icon to the cache, where the save data screen finds them by cart id
fn cache_cart_info(cart_info: &Path, cart_path: &Path) {
    let attr = |name: &str| save::get_attribute(cart_info, name).unwrap_or_default();
    let cart_id = attr("Id");
    if cart_id.trim().is_empty() {
        return;
    }
    let cart_icon = cart_path.join(attr("Icon"));

    let cache_dir = save::get_kazeta_data_dir().join("cache").join(&cart_id);
    let _ = fs::create_dir_all(&cache_dir);
    let _ = fs::copy(cart_info, cache_dir.join("metadata.kzi"));
    if cart_icon.is_file() {
        let _ = fs::copy(&cart_icon, cache_dir.join("icon.png"));
    }
}

fn read_cart_details(cart_info: &Path) -> CartDetails {
    let attr = |name: &str| save::get_attribute(cart_info, name).unwrap_or_default().trim().to_string();
    let icon = attr("Icon");
    let icon = if icon.is_empty() {
        None
    } else {
        fs::read(cart_info.parent().unwrap_or(Path::new("/")).join(icon)).ok()
    };
    CartDetails {
        name: attr("Name"),
        icon,
        id: attr("Id"),
        extends: extended_id(&attr("Extends")),
        has_exec: !attr("Exec").is_empty(),
    }
}

/// Options gamescope needs at startup for the bios: the display menu's resolution and orientation
fn bios_gamescope_args() -> Vec<String> {
    let mut args = resolution_gamescope_args();
    args.extend(orientation_gamescope_args());
    args
}

/// The bios is drawn with pixel scaling
fn bios_runtime_options() -> RuntimeOptions {
    RuntimeOptions { filter: Some(4), ..RuntimeOptions::default() }
}

/// How often the running bios is checked for a cart it handed over
const BIOS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// What the bios left behind: the carts to start, if any, and the bios itself when it is still
/// showing that the cart is starting
struct BiosOutcome {
    status: i32,
    carts: Option<Vec<PathBuf>>,
    bios: Option<RunningBios>,
}

/// Take the carts the bios handed over, if it did: the base cart and its DLC
fn take_boot_cart() -> Option<Vec<PathBuf>> {
    let boot_cart_file = boot_cart_file();
    let carts: Option<Vec<PathBuf>> = fs::read_to_string(&boot_cart_file).ok().map(|contents| {
        contents.lines().map(str::trim).filter(|line| !line.is_empty()).map(PathBuf::from).collect()
    });
    let _ = fs::remove_file(&boot_cart_file);
    carts.filter(|carts| !carts.is_empty())
}

/// Run the bios UI in the gamescope session until it hands over a cart or exits; `ignored_cart` is
/// not started by it until it has been removed
fn run_bios(session: &Session, ignored_cart: Option<&Path>, stop: &dyn Fn() -> bool) -> BiosOutcome {
    session.apply(&bios_runtime_options());
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kazeta"));
    eprintln!("+ {} {} {:?}", exe.display(), UI_ARG, ignored_cart);
    let child = Command::new(exe)
        .env_clear()
        .envs(session.env.iter().map(|(key, value)| (key, value)))
        .arg(UI_ARG)
        .args(ignored_cart)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            eprintln!("Failed to start the bios: {}", e);
            return BiosOutcome { status: 1, carts: None, bios: None };
        }
    };

    loop {
        // the child has not been reaped yet, so its pid cannot have been reused
        if !matches!(child.try_wait(), Ok(None)) {
            return BiosOutcome { status: 0, carts: take_boot_cart(), bios: None };
        }
        // it keeps running, showing that the game is starting, until the game shows a window
        if let Some(carts) = take_boot_cart() {
            return BiosOutcome { status: 0, carts: Some(carts), bios: Some(RunningBios(child)) };
        }
        if stop() {
            drop(RunningBios(child));
            return BiosOutcome { status: 0, carts: None, bios: None };
        }
        thread::sleep(BIOS_POLL_INTERVAL);
    }
}

/// Resolve the runtime file for a cart, falling back to the "none" runtime. For each of
/// `media_paths` in turn, looked for in it, then in the kazeta/runtimes directory and at the root
/// of the external media it is on; then in the runtimes directory of Kazeta's data directory
/// (~/.local/share/kazeta/runtimes).
/// A runtime found on external media is loaded from the data directory instead when it has a file
/// of the same name there, i.e. the same version, as the internal drive is faster to load from.
fn find_runtime(media_paths: &[&Path], runtime_name: &str) -> PathBuf {
    if runtime_name.is_empty() {
        return PathBuf::from(NONE_RUNTIME);
    }

    let internal_dir = save::get_kazeta_data_dir().join("runtimes");
    let mut dirs = Vec::new();
    for media_path in media_paths {
        dirs.push(media_path.to_path_buf());
        if let Some(root) = media_root_of(media_path) {
            dirs.push(root.join(MEDIA_RUNTIMES_DIR));
            dirs.push(root);
        }
    }
    let mut seen = Vec::new();
    for dir in dirs {
        if seen.contains(&dir) {
            continue;
        }
        if let Some(runtime) = find_runtime_in(&dir, runtime_name) {
            let internal = runtime.file_name().map(|name| internal_dir.join(name));
            return match internal {
                Some(internal) if internal.is_file() => internal,
                _ => runtime,
            };
        }
        seen.push(dir);
    }

    find_runtime_in(&internal_dir, runtime_name).unwrap_or_else(|| PathBuf::from(NONE_RUNTIME))
}

/// Find the runtime named `runtime_name` in `dir`
fn find_runtime_in(dir: &Path, runtime_name: &str) -> Option<PathBuf> {
    let exact = dir.join(runtime_name);
    if exact.is_file() {
        return Some(exact);
    }

    let with_ext = dir.join(format!("{}.kzr", runtime_name));
    if with_ext.is_file() {
        return Some(with_ext);
    }

    // versioned runtime, e.g. name-1.0.kzr; only used if it is unambiguous
    let prefix = format!("{}-", runtime_name);
    let mut matches: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    name.starts_with(&prefix) && name.ends_with(".kzr") && p.is_file()
                })
                .collect()
        })
        .unwrap_or_default();
    if matches.len() == 1 {
        return matches.pop();
    }

    None
}

/// Periodically records the end of the current play session until stopped
struct PlaytimeCapture {
    stop: mpsc::Sender<()>,
    handle: thread::JoinHandle<()>,
}

impl PlaytimeCapture {
    fn start(end_file: PathBuf) -> Self {
        let (stop, rx) = mpsc::channel();
        let handle = thread::spawn(move || loop {
            let stopped = !matches!(rx.recv_timeout(PLAYTIME_INTERVAL), Err(mpsc::RecvTimeoutError::Timeout));
            if let Err(e) = fs::write(&end_file, format!("{}\n", now_iso8601())) {
                eprintln!("Failed to write {}: {}", end_file.display(), e);
            }
            if stopped {
                break;
            }
        });
        PlaytimeCapture { stop, handle }
    }

    fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.handle.join();
    }
}

/// Unmounts and tidies up after a cart, also when the launcher bails out early
struct CartCleanup {
    /// cart and DLC packages
    pkg_slots: Vec<Slot>,
    cart_mounted: bool,
    upper: Option<PathBuf>,
}

impl Drop for CartCleanup {
    fn drop(&mut self) {
        // in one go, as each helper run goes through pkexec
        let mut slots = Vec::new();
        if self.cart_mounted {
            slots.extend([Slot::Cart, Slot::Runtime]);
        }
        slots.extend(self.pkg_slots.iter().rev());
        mount::unmount(&slots);
        if let Some(upper) = &self.upper {
            let _ = fs::remove_dir_all(upper.join(".kazeta/share"));
            // only removed if empty
            let _ = fs::remove_dir(upper.join(".kazeta"));
        }
    }
}

/// Run the carts given, a base cart followed by its DLC bottom to top, or the bios when none are
pub fn run(cart_args: &[String]) -> i32 {
    let terminated = signals::install();

    let base_dir = save::get_kazeta_data_dir();
    if let Err(e) = fs::create_dir_all(&base_dir) {
        eprintln!("Failed to create {}: {}", base_dir.display(), e);
    }

    if !cart_args.is_empty() {
        let mut carts = Vec::new();
        for arg in cart_args {
            if !arg.ends_with(".kzi") && !arg.ends_with(".kzp") {
                eprintln!("Not a cart: {} (expected a .kzi or .kzp file)", arg);
                return 1;
            }
            match fs::canonicalize(arg) {
                Ok(cart) => carts.push(cart),
                Err(e) => {
                    eprintln!("Cart {} not found: {}", arg, e);
                    return 1;
                }
            }
        }
        return run_cart(carts, &base_dir, &mut None, None, &|| terminated.get());
    }

    volume::watch();
    let power_key = power::watch();
    let stop = || {
        // powering off while an update is being installed could leave it half written
        if power_key.pressed() && update::running() {
            eprintln!("Power button pressed during an update, ignoring it");
            power_key.clear();
        }
        terminated.get() || power_key.pressed()
    };
    // the cart that was just played, so the bios does not start it again right away
    let mut last_cart: Option<PathBuf> = None;
    // gamescope shared by the bios and games, stopped when dropped
    let mut session: Option<Session> = None;
    loop {
        // a press that stopped the game must not also shut down from the bios
        power_key.clear();

        // started again when it is gone, or the display menu changed what it needs at startup
        let bios_args = bios_gamescope_args();
        if !session.as_mut().is_some_and(|s| s.running() && s.startup_args == bios_args) {
            // the old gamescope has to let go of the display before a new one starts
            drop(session.take());
            session = Session::start(bios_args, &stop);
        }
        let Some(bios_session) = session.as_ref() else {
            eprintln!("Failed to start gamescope for the bios");
            return 1;
        };

        // the bios waits for a cart and leaves its path behind when one should be launched
        let _ = fs::remove_file(boot_cart_file());
        let BiosOutcome { status, carts, bios } = run_bios(bios_session, last_cart.as_deref(), &stop);

        if terminated.get() {
            return 1;
        }
        if power_key.pressed() {
            eprintln!("Power button pressed in the bios, shutting down");
            return 0;
        }
        let Some(carts) = carts else {
            return status;
        };
        // the base cart, whose media counts as removed until it is
        let cart = carts[0].clone();

        // a power button press stops the game and brings us back to the bios
        let status = run_cart(carts, &base_dir, &mut session, bios, &stop);
        eprintln!("Game exited with status {}", status);
        if terminated.get() {
            return 1;
        }
        last_cart = Some(cart);
    }
}

/// A cart or DLC being played
struct Layer {
    /// its cart info file
    info: PathBuf,
    /// the directory with its files
    dir: PathBuf,
    /// where it was found, to look for its runtime
    media_path: PathBuf,
}

impl Layer {
    fn attr(&self, name: &str) -> String {
        save::get_attribute(&self.info, name).unwrap_or_default()
    }
}

/// Mount and run a cart until the game exits or `stop` returns true. `carts` is the base cart
/// followed by its DLC bottom to top, which is mounted on top of it; DLC last in line with its own
/// Exec is what runs, with its own save data if it has its own Id. Otherwise DLC without Exec but
/// with its own Id gets its own save data, and the game is not started when there are several of
/// those. The game runs in `session` when
/// its gamescope options allow it, otherwise the session is stopped for a gamescope of the game's own.
/// `bios` stays on screen until the game shows a window, and is stopped if the game is not started.
fn run_cart(carts: Vec<PathBuf>, base_dir: &Path, session: &mut Option<Session>, bios: Option<RunningBios>, stop: &dyn Fn() -> bool) -> i32 {
    if stop() || carts.is_empty() {
        return 1;
    }
    if carts.len() > mount::MAX_DLC + 1 {
        eprintln!("Too many DLC, at most {} can be played at once", mount::MAX_DLC);
        return 1;
    }
    let is_pkg = |cart: &Path| cart.extension().is_some_and(|ext| ext == "kzp");

    let mut cleanup = CartCleanup { pkg_slots: Vec::new(), cart_mounted: false, upper: None };

    // packages are mounted in one go, as each helper run goes through pkexec
    let pkgs: Vec<(Slot, &Path)> = carts.iter().enumerate()
        .filter(|(_, cart)| is_pkg(cart))
        .map(|(i, cart)| (if i == 0 { Slot::Pkg } else { Slot::Dlc(i - 1) }, cart.as_path()))
        .collect();
    if !pkgs.is_empty() {
        // unmounted by the cleanup even when mounting fails, in case it got partway
        cleanup.pkg_slots = pkgs.iter().map(|(slot, _)| *slot).collect();
        if !mount::mount_pkgs(&pkgs) {
            eprintln!("Failed to mount {:?}", pkgs.iter().map(|(_, pkg)| pkg).collect::<Vec<_>>());
            return 1;
        }
    }

    let mut layers: Vec<Layer> = Vec::new();
    for (i, cart) in carts.iter().enumerate() {
        let layer = if is_pkg(cart) {
            let dir = if i == 0 { Slot::Pkg } else { Slot::Dlc(i - 1) }.path();
            let Some(info) = find_cart_file(&dir, "kzi") else {
                eprintln!("No cart info found in {}", cart.display());
                return 1;
            };
            Layer { info, dir, media_path: cart.parent().unwrap_or(Path::new("/")).to_path_buf() }
        } else {
            let dir = cart.parent().unwrap_or(Path::new("/")).to_path_buf();
            Layer { info: cart.clone(), dir: dir.clone(), media_path: dir }
        };
        layers.push(layer);
    }
    let base = &layers[0];
    let dlc = &layers[1..];
    // DLC with its own Exec runs instead of the base game, and changes what it sets
    let game = dlc.last().filter(|layer| !layer.attr("Exec").is_empty()).unwrap_or(base);
    let attr = |name: &str| match game.attr(name) {
        value if value.is_empty() => base.attr(name),
        value => value,
    };
    for layer in dlc {
        eprintln!("Playing {} with DLC {}", base.info.display(), layer.info.display());
    }

    // DLC with its own Id gets save data of its own: the DLC that runs, otherwise the one DLC
    // without Exec that has one; several of those cannot share one save directory
    let saves_with: Vec<&Layer> = dlc.iter()
        .filter(|layer| layer.attr("Exec").is_empty() && !layer.attr("Id").is_empty())
        .collect();
    let saves_with = if !std::ptr::eq(game, base) && !game.attr("Id").is_empty() {
        game
    } else {
        match saves_with.as_slice() {
            [] => base,
            [layer] => layer,
            layers => {
                eprintln!("Not starting {}: several DLC have an Id of their own, and so their own save data: {}",
                    base.info.display(),
                    layers.iter().map(|layer| layer.info.display().to_string()).collect::<Vec<_>>().join(", "));
                return 1;
            }
        }
    };
    let cart_id = saves_with.attr("Id");
    cache_cart_info(&saves_with.info, &saves_with.dir);

    let lower = base.dir.clone();
    // a cart directory with both the game's and the DLC's cart info holds the files of both, and
    // is mounted once
    let mut dlc_dirs: Vec<&Path> = Vec::new();
    for layer in dlc {
        if layer.dir != lower && !dlc_dirs.contains(&layer.dir.as_path()) {
            dlc_dirs.push(&layer.dir);
        }
    }
    let upper = base_dir.join("saves/default").join(&cart_id);
    // overlayfs' work directory; the mount helper needs it to be the user's. The old kazeta-mount
    // script created run/ as root: Kazeta OS removes it at boot (tmpfiles.d/kazeta.conf), elsewhere
    // it has to be removed by hand
    let work = base_dir.join("run/work");

    if !upper.is_dir() {
        let _ = fs::remove_file(&upper);
        let _ = fs::create_dir_all(&upper);
    }

    let runtime = find_runtime(&[game.media_path.as_path(), base.media_path.as_path()], &attr("Runtime"));

    cleanup.cart_mounted = true;
    cleanup.upper = Some(upper.clone());
    let Some(target) = mount::mount_cart(&lower, &dlc_dirs, &upper, &work, &runtime) else {
        eprintln!("Failed to mount cart {}", cart_id);
        return 1;
    };

    // home directory for the game, only applied to the game's environment below
    let cart_home = target;

    let cart_exec = attr("Exec");
    // directory Exec runs from, relative to the cart root; the runtime changes to it
    let cart_path_dir = attr("Path");
    let cart_gsopts: Vec<String> = match attr("GamescopeOptions") {
        opts if opts.is_empty() => resolution_gamescope_args(),
        opts => opts.split_whitespace().map(String::from).collect(),
    };
    let cart_exec_file = cart_exec_file();
    let _ = fs::write(&cart_exec_file, format!("{}\n", cart_exec));
    // environment variables the runtime sets for the game (Linux) or for Proton (Windows), not gamescope
    let cart_env = match save::get_attributes(&game.info, "Env").unwrap_or_default() {
        env if env.is_empty() => save::get_attributes(&base.info, "Env").unwrap_or_default(),
        env => env,
    };
    let cart_env_file = cart_env_file();
    let _ = fs::remove_file(&cart_env_file);
    let cart_env_file = if cart_env.is_empty() {
        None
    } else {
        let _ = fs::write(&cart_env_file, cart_env.iter().map(|var| format!("{}\n", var)).collect::<String>());
        Some(cart_env_file)
    };

    let var_dir = cart_home.join(".kazeta/var");
    let playtime_start = var_dir.join("playtime_start");
    let playtime_end = var_dir.join("playtime_end");

    // append previously captured playtime to the log
    if let (Ok(start), Ok(end)) = (fs::read_to_string(&playtime_start), fs::read_to_string(&playtime_end)) {
        let entry = format!("{} {}\n", start.trim(), end.trim());
        let appended = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(var_dir.join("playtime.log"))
            .and_then(|mut log| log.write_all(entry.as_bytes()));
        if let Err(e) = appended {
            eprintln!("Failed to append playtime log: {}", e);
        }
    }
    let _ = fs::remove_file(&playtime_start);
    let _ = fs::remove_file(&playtime_end);

    let _ = fs::create_dir_all(&var_dir);
    let _ = fs::write(&playtime_start, format!("{}\n", now_iso8601()));
    let playtime = PlaytimeCapture::start(playtime_end);

    // skip the game if we were asked to stop while mounting; cleanup still runs
    let game = Game { cart_home: &cart_home, cart_exec_file: &cart_exec_file, cart_path_dir: &cart_path_dir, cart_env_file: cart_env_file.as_deref() };
    let status = if stop() { 1 } else { run_game(&game, &cart_gsopts, session, bios, stop) };

    playtime.stop();
    drop(cleanup);

    status
}

/// What is needed to start a cart's game
struct Game<'a> {
    cart_home: &'a Path,
    cart_exec_file: &'a Path,
    cart_path_dir: &'a str,
    cart_env_file: Option<&'a Path>,
}

impl Game<'_> {
    /// Arguments for the runtime's run script. The path and env file are only passed when set, so
    /// runtimes without support for them see the same arguments; the env file comes after the path,
    /// which is then "." when the cart has none.
    fn run_args(&self) -> Vec<&OsStr> {
        let mut run_args: Vec<&OsStr> = vec![OsStr::new("./.kazeta/share/run"), self.cart_exec_file.as_os_str()];
        if !self.cart_path_dir.is_empty() || self.cart_env_file.is_some() {
            run_args.push(OsStr::new(if self.cart_path_dir.is_empty() { "." } else { self.cart_path_dir }));
        }
        if let Some(cart_env_file) = self.cart_env_file {
            run_args.push(cart_env_file.as_os_str());
        }
        run_args
    }

    /// The game's home and XDG directories, in the cart's writable home
    fn configure(&self, command: &mut Command) {
        let cart_home = self.cart_home;
        command
            .current_dir(cart_home)
            .env("HOME", cart_home)
            .env("XDG_CONFIG_HOME", cart_home.join(".config"))
            .env("XDG_DATA_HOME", cart_home.join(".local/share"))
            .env("XDG_CACHE_HOME", cart_home.join(".cache"))
            .env("XDG_STATE_HOME", cart_home.join(".local/state"))
            // keep GLib from loading gvfs, which writes file metadata to .local/share/gvfs-metadata
            // in the cart's home; games have no use for gvfs (network shares, trash, metadata)
            .env("GIO_USE_VFS", "local")
            .stdout(Stdio::piped());
    }
}

fn run_game(game: &Game, cart_gsopts: &[String], session: &mut Option<Session>, bios: Option<RunningBios>, stop: &dyn Fn() -> bool) -> i32 {
    let (options, mut startup_args) = session::split_options(cart_gsopts);
    startup_args.extend(orientation_gamescope_args());

    let fits_session = session.as_mut().is_some_and(|s| s.running() && s.startup_args == startup_args);
    if let Some(current) = session.as_ref().filter(|_| fits_session) {
        current.prepare_game_display(&options);
        // unset options get gamescope's defaults rather than what the bios uses, applied once the
        // bios has made way for the game
        let handoff = match bios {
            Some(bios) => Some(current.hand_off(bios, options)),
            None => {
                current.apply(&options);
                None
            }
        };
        let status = run_game_in_session(game, current, stop);
        drop(handoff);
        return status;
    }

    // the game needs gamescope started with its options; the bios gets the session back afterwards
    drop(bios);
    *session = None;
    run_game_in_own_gamescope(game, cart_gsopts, stop)
}

/// Run the game on the games' Xwayland server of the shared gamescope session
fn run_game_in_session(game: &Game, session: &Session, stop: &dyn Fn() -> bool) -> i32 {
    let run_args = game.run_args();
    let display = &session.game_display;
    eprintln!("+ DISPLAY={} gamescopereaper -- {:?}", display, run_args);
    let mut command = Command::new("gamescopereaper");
    command
        .env_clear()
        .envs(session.env.iter().map(|(key, value)| (key, value)))
        .env("DISPLAY", display)
        // kills everything the game started once the game exits or is stopped
        .arg("--")
        .args(&run_args);
    game.configure(&mut command);
    wait_game(game, command.spawn(), stop)
}

/// Run the game in a gamescope started with its options
fn run_game_in_own_gamescope(game: &Game, cart_gsopts: &[String], stop: &dyn Fn() -> bool) -> i32 {
    let run_args = game.run_args();
    eprintln!("+ gamescope {:?} -- {:?}", cart_gsopts, run_args);
    let mut command = Command::new("gamescope");
    command
        .args(cart_gsopts)
        .args(orientation_gamescope_args())
        .arg("--")
        .args(&run_args);
    game.configure(&mut command);
    wait_game(game, command.spawn(), stop)
}

/// Wait for the started game to exit, logging its output to the cart's run log
fn wait_game(game: &Game, child: io::Result<std::process::Child>, stop: &dyn Fn() -> bool) -> i32 {
    let cart_home = game.cart_home;
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            eprintln!("Failed to start the game: {}", e);
            return 1;
        }
    };

    // game output goes to the cart's run log, minus pressure-vessel noise
    let stdout = child.stdout.take();
    let log_path = cart_home.join(".kazeta/var/run.log");
    let log_writer = stdout.map(|stdout| thread::spawn(move || {
        let mut log = fs::File::create(&log_path)
            .map_err(|e| eprintln!("Failed to create {}: {}", log_path.display(), e))
            .ok();
        // keep reading even without a log so the game never sees a closed pipe
        for line in BufReader::new(stdout).lines().map_while(io::Result::ok) {
            if let Some(log) = log.as_mut() {
                if !line.contains("pressure-vessel-wrap") {
                    let _ = writeln!(log, "{}", line);
                }
            }
        }
    }));

    let status = signals::wait_child(&mut child, stop, GAMESCOPE_KILL_TIMEOUT);
    if let Some(log_writer) = log_writer {
        let _ = log_writer.join();
    }

    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cart(id: &str, extends: Option<&str>, has_exec: bool) -> CartDetails {
        CartDetails {
            name: id.to_string(),
            icon: None,
            id: id.to_string(),
            extends: extends.and_then(extended_id),
            has_exec,
        }
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn reads_the_base_game_id_from_extends() {
        assert_eq!(extended_id("game:abc123").as_deref(), Some("game"));
        assert_eq!(extended_id(" game ").as_deref(), Some("game"));
        assert_eq!(extended_id(""), None);
        assert_eq!(extended_id(":abc123"), None);
    }

    #[test]
    fn plays_dlc_with_its_base_game() {
        let carts = vec![
            (PathBuf::from("game.kzp"), cart("game", None, true)),
            (PathBuf::from("dlc1.kzp"), cart("", Some("game:hash"), false)),
            (PathBuf::from("expansion.kzp"), cart("expansion", Some("game:hash"), true)),
            (PathBuf::from("dlc2.kzp"), cart("dlc2", Some("game:hash"), false)),
            (PathBuf::from("orphan.kzp"), cart("", Some("missing:hash"), true)),
            (PathBuf::from("other.kzp"), cart("other", None, true)),
        ];
        let entries = launch_entries(&carts);
        let carts: Vec<Vec<PathBuf>> = entries.iter().map(|entry| entry.carts.clone()).collect();
        assert_eq!(carts, vec![
            paths(&["game.kzp", "dlc1.kzp", "dlc2.kzp"]),
            paths(&["game.kzp", "dlc1.kzp", "dlc2.kzp", "expansion.kzp"]),
            paths(&["other.kzp"]),
        ]);
        assert_eq!(entries[1].name, "expansion");
    }

    #[test]
    fn base_game_without_id_has_no_dlc() {
        let carts = vec![
            (PathBuf::from("game.kzp"), cart("", None, true)),
            (PathBuf::from("dlc.kzp"), cart("", Some(""), false)),
        ];
        let carts: Vec<Vec<PathBuf>> = launch_entries(&carts).into_iter().map(|entry| entry.carts).collect();
        // an empty Extends is not DLC
        assert_eq!(carts, vec![paths(&["game.kzp"]), paths(&["dlc.kzp"])]);
    }
}

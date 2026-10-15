// Cart launcher: mounts a cart and runs it under gamescope.
// Without a cart argument the bios UI is started first; it waits for a cart and hands it back here,
// and is started again once the game exits. Pressing the power button stops a running game, or
// shuts down when pressed in the bios.
// Session duties (controllers, audio, logging, power off) are left to kazeta-session.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use chrono::{Local, SecondsFormat};

use crate::save;
use crate::power;
use crate::signals;
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

/// Written by the bios UI with the path of the cart to launch once it exits
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

/// Find all inserted carts, cart packages first
pub fn find_carts() -> Vec<PathBuf> {
    let base_ext = PathBuf::from(save::get_external_media_dir());
    let mut carts = Vec::new();
    for extension in ["kzp", "kzi"] {
        let mut files = save::find_files_by_extension(&base_ext, extension, 1, false).unwrap_or_default();
        files.sort();
        carts.extend(files);
    }
    carts
}

/// What the cart selection shows for a cart
pub struct CartDetails {
    pub name: String,
    /// contents of the icon file, if the cart has one
    pub icon: Option<Vec<u8>>,
}

/// Read the name and icon of a cart for display. A cart package is briefly mounted to read
/// its cart info. Falls back to the file name when there is no name.
pub fn cart_details(cart: &Path) -> CartDetails {
    let is_pkg = cart.extension().is_some_and(|ext| ext == "kzp");
    let mut details = if is_pkg {
        // separate from the mount point used to run a cart
        let preview_mount = save::get_kazeta_data_dir().join("run/preview");
        pkexec_mount(&[Path::new("kzp"), cart, &preview_mount]);
        let details = find_cart_file(&preview_mount, "kzi").map(|info| read_cart_details(&info));
        pkexec_mount(&[Path::new("kzp"), Path::new("--unmount"), &preview_mount]);
        details
    } else {
        Some(read_cart_details(cart))
    }
    .unwrap_or(CartDetails { name: String::new(), icon: None });

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
    CartDetails { name: attr("Name"), icon }
}

fn pkexec_mount(args: &[&Path]) {
    eprintln!("+ pkexec kazeta-mount {:?}", args);
    match Command::new("pkexec").arg("kazeta-mount").args(args).status() {
        Ok(status) if !status.success() => eprintln!("kazeta-mount failed: {}", status),
        Err(e) => eprintln!("Failed to run kazeta-mount: {}", e),
        _ => {}
    }
}

/// Run the bios UI; `ignored_cart` is not started by it until it has been removed
fn run_bios(resolution_args: &[String], ignored_cart: Option<&Path>, stop: &dyn Fn() -> bool) -> i32 {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kazeta"));
    eprintln!("+ gamescope {:?} --filter pixel -- {} {} {:?}", resolution_args, exe.display(), UI_ARG, ignored_cart);
    let child = Command::new("gamescope")
        .args(resolution_args)
        .args(orientation_gamescope_args())
        .args(["--filter", "pixel", "--"])
        .arg(exe)
        .arg(UI_ARG)
        .args(ignored_cart)
        .spawn();
    match child {
        Ok(mut child) => {
            let _ = signals::wait_child(&mut child, stop, GAMESCOPE_KILL_TIMEOUT);
            0
        }
        Err(e) => {
            eprintln!("Failed to start gamescope: {}", e);
            1
        }
    }
}

/// Resolve the runtime file for a cart, falling back to the "none" runtime
fn find_runtime(media_path: &Path, runtime_name: &str) -> PathBuf {
    if runtime_name.is_empty() {
        return PathBuf::from(NONE_RUNTIME);
    }

    let exact = media_path.join(runtime_name);
    if exact.is_file() {
        return exact;
    }

    let with_ext = media_path.join(format!("{}.kzr", runtime_name));
    if with_ext.is_file() {
        return with_ext;
    }

    // versioned runtime, e.g. name-1.0.kzr; only used if it is unambiguous
    let prefix = format!("{}-", runtime_name);
    let matches: Vec<PathBuf> = fs::read_dir(media_path)
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
        return matches[0].clone();
    }

    PathBuf::from(NONE_RUNTIME)
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
    // kzp mount point, or the cart's own directory; unmounted either way like the original launcher
    pkg_mount: Option<PathBuf>,
    mounted: Option<(PathBuf, PathBuf)>,
    upper: Option<PathBuf>,
    cart_home: Option<PathBuf>,
}

impl Drop for CartCleanup {
    fn drop(&mut self) {
        if let Some(cart_home) = &self.cart_home {
            let _ = fs::remove_file(cart_home.join(".local/share/gvfs-metadata"));
        }
        if let Some((target, runtimedir)) = &self.mounted {
            pkexec_mount(&[Path::new("--unmount"), target, runtimedir]);
        }
        if let Some(pkg_mount) = &self.pkg_mount {
            pkexec_mount(&[Path::new("kzp"), Path::new("--unmount"), pkg_mount]);
        }
        if let Some(upper) = &self.upper {
            let _ = fs::remove_dir_all(upper.join(".kazeta/share"));
            // only removed if empty
            let _ = fs::remove_dir(upper.join(".kazeta"));
        }
    }
}

pub fn run(cart_arg: Option<&str>) -> i32 {
    let terminated = signals::install();

    let base_dir = save::get_kazeta_data_dir();
    if let Err(e) = fs::create_dir_all(&base_dir) {
        eprintln!("Failed to create {}: {}", base_dir.display(), e);
    }

    if let Some(arg) = cart_arg {
        if !arg.ends_with(".kzi") && !arg.ends_with(".kzp") {
            eprintln!("Not a cart: {} (expected a .kzi or .kzp file)", arg);
            return 1;
        }
        let cart = match fs::canonicalize(arg) {
            Ok(cart) => cart,
            Err(e) => {
                eprintln!("Cart {} not found: {}", arg, e);
                return 1;
            }
        };
        return run_cart(cart, &base_dir, &|| terminated.get());
    }

    volume::watch();
    let power_key = power::watch();
    let stop = || terminated.get() || power_key.pressed();
    // the cart that was just played, so the bios does not start it again right away
    let mut last_cart: Option<PathBuf> = None;
    loop {
        // a press that stopped the game must not also shut down from the bios
        power_key.clear();

        // the bios waits for a cart and leaves its path behind when one should be launched
        let boot_cart_file = boot_cart_file();
        let _ = fs::remove_file(&boot_cart_file);
        let status = run_bios(&resolution_gamescope_args(), last_cart.as_deref(), &stop);
        let cart = fs::read_to_string(&boot_cart_file).ok().map(|path| PathBuf::from(path.trim()));
        let _ = fs::remove_file(&boot_cart_file);

        if terminated.get() {
            return 1;
        }
        if power_key.pressed() {
            eprintln!("Power button pressed in the bios, shutting down");
            return 0;
        }
        let Some(cart) = cart else {
            return status;
        };

        // a power button press stops the game and brings us back to the bios
        let status = run_cart(cart.clone(), &base_dir, &stop);
        eprintln!("Game exited with status {}", status);
        if terminated.get() {
            return 1;
        }
        last_cart = Some(cart);
    }
}

/// Mount and run a cart until the game exits or `stop` returns true
fn run_cart(cart: PathBuf, base_dir: &Path, stop: &dyn Fn() -> bool) -> i32 {
    if stop() {
        return 1;
    }
    let is_pkg = cart.extension().is_some_and(|ext| ext == "kzp");

    let mut cleanup = CartCleanup { pkg_mount: None, mounted: None, upper: None, cart_home: None };

    let (cart_info, cart_path, media_path) = if is_pkg {
        let cart_pkg = cart;
        let cart_path = base_dir.join("run/pkg");
        pkexec_mount(&[Path::new("kzp"), &cart_pkg, &cart_path]);
        cleanup.pkg_mount = Some(cart_path.clone());
        let Some(cart_info) = find_cart_file(&cart_path, "kzi") else {
            eprintln!("No cart info found in {}", cart_pkg.display());
            return 1;
        };
        let media_path = cart_pkg.parent().unwrap_or(Path::new("/")).to_path_buf();
        (cart_info, cart_path, media_path)
    } else {
        let cart_info = cart;
        let cart_path = cart_info.parent().unwrap_or(Path::new("/")).to_path_buf();
        cleanup.pkg_mount = Some(cart_path.clone());
        (cart_info.clone(), cart_path.clone(), cart_path)
    };

    let attr = |name: &str| save::get_attribute(&cart_info, name).unwrap_or_default();
    let cart_id = attr("Id");
    cache_cart_info(&cart_info, &cart_path);

    let lower = cart_path.clone();
    let upper = base_dir.join("saves/default").join(&cart_id);
    let work = base_dir.join("run/work");
    let target = base_dir.join("run/cart");
    let runtimedir = base_dir.join("run/runtime");

    if !upper.is_dir() {
        let _ = fs::remove_file(&upper);
        let _ = fs::create_dir_all(&upper);
    }

    // workaround for gvfs-metadata preventing unmount
    let _ = fs::create_dir_all(upper.join(".local/share"));
    let _ = fs::File::create(upper.join(".local/share/gvfs-metadata"));

    let runtime = find_runtime(&media_path, &attr("Runtime"));

    pkexec_mount(&[&lower, &upper, &work, &target, &runtime, &runtimedir]);
    cleanup.mounted = Some((target.clone(), runtimedir.clone()));
    cleanup.upper = Some(upper.clone());

    // home directory for the game, only applied to the game's environment below
    let cart_home = target.clone();
    cleanup.cart_home = Some(cart_home.clone());

    let cart_exec = attr("Exec");
    let cart_gsopts: Vec<String> = match attr("GamescopeOptions") {
        opts if opts.is_empty() => resolution_gamescope_args(),
        opts => opts.split_whitespace().map(String::from).collect(),
    };
    let cart_exec_file = cart_exec_file();
    let _ = fs::write(&cart_exec_file, format!("{}\n", cart_exec));

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
    let status = if stop() { 1 } else { run_game(&cart_home, &cart_gsopts, &cart_exec_file, stop) };

    playtime.stop();
    drop(cleanup);

    status
}

fn run_game(cart_home: &Path, cart_gsopts: &[String], cart_exec_file: &Path, stop: &dyn Fn() -> bool) -> i32 {
    eprintln!("+ gamescope {:?} -- ./.kazeta/share/run {}", cart_gsopts, cart_exec_file.display());
    let child = Command::new("gamescope")
        .args(cart_gsopts)
        .args(orientation_gamescope_args())
        .arg("--")
        .arg("./.kazeta/share/run")
        .arg(cart_exec_file)
        .current_dir(cart_home)
        .env("HOME", cart_home)
        .env("XDG_CONFIG_HOME", cart_home.join(".config"))
        .env("XDG_DATA_HOME", cart_home.join(".local/share"))
        .env("XDG_CACHE_HOME", cart_home.join(".cache"))
        .env("XDG_STATE_HOME", cart_home.join(".local/state"))
        .stdout(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            eprintln!("Failed to start gamescope: {}", e);
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

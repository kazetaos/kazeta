// OS updates from external media: a system image named kazeta-*.img.tar.xz at the top level of
// a drive is offered for install and deployed with frzr-deploy, which reports progress with
// --install-progress as whole percentages on stderr, one per line.

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::launcher;
use crate::save;

const UPDATE_PREFIX: &str = "kazeta-";
const UPDATE_SUFFIX: &str = ".img.tar.xz";
const DEPLOYMENTS_DIR: &str = "/frzr_root/deployments";

/// Exists while an update is being installed, so the launcher ignores the power button
fn running_file() -> PathBuf {
    launcher::runtime_dir().join("kazeta-updating")
}

/// Path of an update the user chose not to install; it is not offered again until it is removed
fn declined_file() -> PathBuf {
    launcher::runtime_dir().join("kazeta-update-declined")
}

pub fn running() -> bool {
    running_file().exists()
}

/// Name of the deployment an update file creates: the file name up to the first '.', like frzr-deploy
pub fn deployment_name(update: &Path) -> String {
    let name = update.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    name.split('.').next().unwrap_or_default().to_string()
}

fn is_update_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    name.starts_with(UPDATE_PREFIX) && name.ends_with(UPDATE_SUFFIX) && path.is_file()
}

/// Find the update files at the top level of the inserted drives that are not installed yet
/// and were not declined, highest name first
pub fn find_updates() -> Vec<PathBuf> {
    let base_ext = PathBuf::from(save::get_external_media_dir());
    let mut updates: Vec<PathBuf> = fs::read_dir(&base_ext)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|drive| fs::read_dir(drive.path()).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_update_file(path))
        // already running or waiting for the next boot
        .filter(|path| !Path::new(DEPLOYMENTS_DIR).join(deployment_name(path)).exists())
        .collect();

    let declined_file = declined_file();
    if let Ok(declined) = fs::read_to_string(&declined_file) {
        let declined: Vec<PathBuf> = declined.lines().map(PathBuf::from).collect();
        if declined.iter().any(|path| updates.contains(path)) {
            updates.retain(|update| !declined.contains(update));
        } else {
            // removed, offer them again when they are inserted next time
            let _ = fs::remove_file(&declined_file);
        }
    }

    updates.sort();
    updates.reverse();
    updates
}

/// Do not offer `updates` again until they are removed
pub fn decline(updates: &[PathBuf]) {
    let declined_file = declined_file();
    let paths: Vec<String> = updates.iter().map(|path| path.to_string_lossy().into_owned()).collect();
    if let Err(e) = fs::write(&declined_file, paths.join("\n")) {
        eprintln!("Failed to write {}: {}", declined_file.display(), e);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UpdateStatus {
    /// percent installed
    Running(u8),
    Done,
    Failed,
}

/// Install `update` in the background
pub fn install(update: PathBuf) -> Arc<Mutex<UpdateStatus>> {
    let status = Arc::new(Mutex::new(UpdateStatus::Running(0)));
    let thread_status = status.clone();
    thread::spawn(move || {
        let _ = fs::write(running_file(), b"");
        let result = if run_deploy(&update, &thread_status) { UpdateStatus::Done } else { UpdateStatus::Failed };
        let _ = fs::remove_file(running_file());
        if let Ok(mut status) = thread_status.lock() {
            *status = result;
        }
    });
    status
}

fn run_deploy(update: &Path, status: &Mutex<UpdateStatus>) -> bool {
    eprintln!("+ pkexec frzr-deploy --install-progress {}", update.display());
    let child = Command::new("pkexec")
        .args(["frzr-deploy", "--install-progress"])
        .arg(update)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            eprintln!("Failed to run frzr-deploy: {}", e);
            return false;
        }
    };

    if let Some(stderr) = child.stderr.take() {
        for line in BufReader::new(stderr).lines().map_while(io::Result::ok) {
            match line.trim().parse::<u8>() {
                Ok(percent) => {
                    if let Ok(mut status) = status.lock() {
                        *status = UpdateStatus::Running(percent.min(100));
                    }
                }
                // anything else is a message for the log
                Err(_) => eprintln!("{}", line),
            }
        }
    }

    match child.wait() {
        Ok(exit) if exit.success() => true,
        Ok(exit) => {
            eprintln!("frzr-deploy failed: {}", exit);
            false
        }
        Err(e) => {
            eprintln!("Failed to wait for frzr-deploy: {}", e);
            false
        }
    }
}

pub fn reboot() {
    eprintln!("+ systemctl reboot");
    if let Err(e) = Command::new("systemctl").arg("reboot").status() {
        eprintln!("Failed to reboot: {}", e);
    }
}

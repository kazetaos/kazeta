// Cart mounts. Mounting needs root, so the bios asks the system's kazeta binary to do it through
// pkexec: `kazeta --mount-helper <command> ...`, allowed without a password by a polkit rule.
//
// The helper is kept narrow, since it runs as root on behalf of whoever is at the machine:
// - it only mounts on, and unmounts, its own mount points under /run/kazeta/<uid>, which are
//   owned by root, so the caller cannot point them elsewhere
// - every path it is given is opened as the calling user (with the user's groups), so it only
//   reaches what the user could already read; the save and work directories must be the user's own
// - the opened files are handed to the kernel as /proc/self/fd paths, so a path cannot be swapped
//   for another between being checked and being mounted
// - everything is mounted nosuid,nodev, so an image cannot bring setuid files or device nodes along
// - it never deletes anything: overlayfs clears out its own work directory on mount
//
// The overlay is still mounted by root, so metacopy works and files are copied up the same way as
// before. Images are EROFS, mounted straight from the file, which needs kernel 6.12 or later
// built with EROFS_FS_BACKED_BY_FILE.

use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Argument that runs the binary as the mount helper
pub const HELPER_ARG: &str = "--mount-helper";

/// The binary pkexec runs as the helper. Always the system's, never a custom build in the user's
/// data dir: polkit must only authorize a file the user cannot change. Packagers installing
/// elsewhere set KAZETA_MOUNT_HELPER at build time, and point the polkit rule at the same path.
const HELPER: &str = match option_env!("KAZETA_MOUNT_HELPER") {
    Some(path) => path,
    None => "/usr/bin/kazeta",
};

const MOUNT_ROOT: &str = "/run/kazeta";

/// The helper's mount points, under /run/kazeta/<uid>
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slot {
    /// a cart package briefly mounted to show its name and icon
    Preview,
    /// the cart package being played
    Pkg,
    /// the runtime of the cart being played
    Runtime,
    /// the overlay the cart is played from: the cart and runtime, with the save data on top
    Cart,
}

impl Slot {
    const ALL: [Slot; 4] = [Slot::Preview, Slot::Pkg, Slot::Runtime, Slot::Cart];

    fn name(self) -> &'static str {
        match self {
            Slot::Preview => "preview",
            Slot::Pkg => "pkg",
            Slot::Runtime => "runtime",
            Slot::Cart => "cart",
        }
    }

    fn from_name(name: &str) -> Option<Slot> {
        Slot::ALL.into_iter().find(|slot| slot.name() == name)
    }

    fn path_for(self, uid: u32) -> PathBuf {
        Path::new(MOUNT_ROOT).join(uid.to_string()).join(self.name())
    }

    /// Where this slot is mounted for the current user
    pub fn path(self) -> PathBuf {
        self.path_for(unsafe { libc::getuid() })
    }
}

fn run_helper(args: &[&OsStr]) -> bool {
    eprintln!("+ pkexec {} {} {:?}", HELPER, HELPER_ARG, args);
    match Command::new("pkexec").arg(HELPER).arg(HELPER_ARG).args(args).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("Mount helper failed: {}", status);
            false
        }
        Err(e) => {
            eprintln!("Failed to run mount helper: {}", e);
            false
        }
    }
}

/// Mount a cart package read-only on `slot` (Preview or Pkg), returning where it is mounted
pub fn mount_pkg(slot: Slot, pkg: &Path) -> Option<PathBuf> {
    run_helper(&["mount-pkg".as_ref(), slot.name().as_ref(), pkg.as_os_str()]).then(|| slot.path())
}

/// Mount `runtime` and an overlay of the cart in `lower` and the runtime, with the save data in
/// `upper` on top, returning where the cart is mounted. `work` is created if missing, and must be
/// on the same filesystem as `upper`.
pub fn mount_cart(lower: &Path, upper: &Path, work: &Path, runtime: &Path) -> Option<PathBuf> {
    if let Err(e) = fs::create_dir_all(work) {
        eprintln!("Failed to create {}: {}", work.display(), e);
        return None;
    }
    let args = ["mount-cart".as_ref(), lower.as_os_str(), upper.as_os_str(), work.as_os_str(), runtime.as_os_str()];
    run_helper(&args).then(|| Slot::Cart.path())
}

/// Unmount the given slots, in order
pub fn unmount(slots: &[Slot]) {
    if slots.is_empty() {
        return;
    }
    let mut args: Vec<&OsStr> = vec!["unmount".as_ref()];
    args.extend(slots.iter().map(|slot| OsStr::new(slot.name())));
    run_helper(&args);
}

/// Entry point of the helper, run as root by pkexec
pub fn helper_main(args: &[String]) -> i32 {
    match helper(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("kazeta mount helper: {}", e);
            1
        }
    }
}

fn helper(args: &[String]) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(error("must be run as root, through pkexec"));
    }
    // set by pkexec, which clears the rest of the environment; only root can fake it
    let uid: u32 = std::env::var("PKEXEC_UID")
        .ok()
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| error("PKEXEC_UID is not set, run through pkexec"))?;
    let user = User::lookup(uid)?;

    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["mount-pkg", slot, pkg] => {
            let slot = Slot::from_name(slot).filter(|slot| matches!(slot, Slot::Preview | Slot::Pkg))
                .ok_or_else(|| error("cart packages are only mounted on preview or pkg"))?;
            let pkg = user.as_user(|| open_image(Path::new(pkg)))?;
            let target = prepare_mount_point(uid, slot)?;
            mount_image(&pkg, &target)
        }
        ["mount-cart", lower, upper, work, runtime] => {
            let (lower, upper, work, runtime) = user.as_user(|| {
                Ok((
                    open_dir(Path::new(lower), None)?,
                    open_dir(Path::new(upper), Some(uid))?,
                    open_dir(Path::new(work), Some(uid))?,
                    open_image(Path::new(runtime))?,
                ))
            })?;
            let runtime_dir = prepare_mount_point(uid, Slot::Runtime)?;
            let target = prepare_mount_point(uid, Slot::Cart)?;
            mount_image(&runtime, &runtime_dir)?;

            // metacopy=on reduces the pressure vessel writes
            // xino=off fixes 32-bit applications crashing
            let options = format!(
                "lowerdir={}:{},upperdir={},workdir={},metacopy=on,xino=off",
                fd_path(&lower),
                runtime_dir.display(),
                fd_path(&upper),
                fd_path(&work),
            );
            mount("overlay", &target, "overlay", libc::MS_NOSUID | libc::MS_NODEV, Some(&options)).inspect_err(|_| {
                let _ = unmount_path(&runtime_dir);
            })
        }
        ["unmount", slots @ ..] if !slots.is_empty() => {
            let slots = slots.iter()
                .map(|name| Slot::from_name(name).ok_or_else(|| error(&format!("unknown mount point {}", name))))
                .collect::<io::Result<Vec<_>>>()?;
            // keep going, so one busy mount does not leave the others behind
            let mut result = Ok(());
            for slot in slots {
                if let Err(e) = unmount_path(&slot.path_for(uid)) {
                    eprintln!("kazeta mount helper: failed to unmount {}: {}", slot.name(), e);
                    result = Err(e);
                }
            }
            result
        }
        _ => Err(error("usage: kazeta --mount-helper mount-pkg <preview|pkg> <image> | mount-cart <lower> <upper> <work> <runtime> | unmount <slot>...")),
    }
}

fn error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, message)
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| error("path contains a NUL byte"))
}

/// The path the kernel resolves to an already opened file
fn fd_path(file: &File) -> String {
    format!("/proc/self/fd/{}", file.as_raw_fd())
}

/// The calling user's identity, to open their files with
struct User {
    uid: u32,
    gid: u32,
    groups: Vec<libc::gid_t>,
}

impl User {
    fn lookup(uid: u32) -> io::Result<User> {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; 16384];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let ret = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut found) };
        if ret != 0 || found.is_null() {
            return Err(error(&format!("unknown user {}", uid)));
        }
        let gid = pwd.pw_gid;
        let name = unsafe { CStr::from_ptr(pwd.pw_name) }.to_owned();

        let mut groups: Vec<libc::gid_t> = vec![0; 64];
        loop {
            let mut count = groups.len() as libc::c_int;
            let ret = unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut count) };
            if ret >= 0 {
                groups.truncate(count as usize);
                break;
            }
            if groups.len() >= 65536 {
                return Err(error("too many groups"));
            }
            groups.resize((count as usize).max(groups.len() * 2), 0);
        }
        Ok(User { uid, gid, groups })
    }

    /// Run `f` with file access checked as this user rather than root
    fn as_user<T>(&self, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        // only the root groups pkexec gave us need restoring; root's file access ignores them anyway
        let mut saved: Vec<libc::gid_t> = vec![0; 256];
        let count = unsafe { libc::getgroups(saved.len() as libc::c_int, saved.as_mut_ptr()) };
        check(count)?;
        saved.truncate(count as usize);

        check(unsafe { libc::setgroups(self.groups.len(), self.groups.as_ptr()) })?;
        unsafe {
            libc::setfsgid(self.gid);
            libc::setfsuid(self.uid);
        }
        // setfsuid and setfsgid report the previous ids rather than errors, so check they took
        let switched = unsafe { libc::setfsuid(u32::MAX) as u32 == self.uid && libc::setfsgid(u32::MAX) as u32 == self.gid };

        let result = if switched { f() } else { Err(error("failed to switch to the calling user")) };

        unsafe {
            libc::setfsuid(0);
            libc::setfsgid(0);
        }
        check(unsafe { libc::setgroups(saved.len(), saved.as_ptr()) })?;
        if unsafe { libc::setfsuid(u32::MAX) } != 0 {
            // should not happen, but never carry on with mixed up ids
            std::process::abort();
        }
        result
    }
}

/// Open a cart package or runtime image, which must be a regular file
fn open_image(path: &Path) -> io::Result<File> {
    let file = File::open(path).map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path.display(), e)))?;
    if !file.metadata()?.is_file() {
        return Err(error(&format!("{} is not a file", path.display())));
    }
    Ok(file)
}

/// Open a directory, which must be owned by `owner` if given; a symlink to a directory is only
/// followed when no owner is required
fn open_dir(path: &Path, owner: Option<u32>) -> io::Result<File> {
    let mut flags = libc::O_DIRECTORY;
    if owner.is_some() {
        flags |= libc::O_NOFOLLOW;
    }
    let dir = OpenOptions::new().read(true).custom_flags(flags).open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path.display(), e)))?;
    if let Some(owner) = owner {
        if dir.metadata()?.uid() != owner {
            return Err(error(&format!("{} is not owned by the calling user", path.display())));
        }
    }
    Ok(dir)
}

/// Create root's directory for the user's mount points if needed, and clear whatever is still
/// mounted on `slot` from before
fn prepare_mount_point(uid: u32, slot: Slot) -> io::Result<PathBuf> {
    let target = slot.path_for(uid);
    for dir in [Path::new(MOUNT_ROOT), target.parent().unwrap(), &target] {
        match fs::DirBuilder::new().mode(0o755).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let metadata = fs::symlink_metadata(dir)?;
        // only root can write to /run, so these are ours, unless something else claimed the name
        if !metadata.is_dir() || metadata.uid() != 0 {
            return Err(error(&format!("{} is not a directory owned by root", dir.display())));
        }
    }
    // stacked mounts come off one at a time
    for _ in 0..8 {
        let path = cstring(&target)?;
        if unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH | libc::UMOUNT_NOFOLLOW) } != 0 {
            break;
        }
    }
    Ok(target)
}

fn mount(source: &str, target: &Path, fstype: &str, flags: libc::c_ulong, data: Option<&str>) -> io::Result<()> {
    let source = CString::new(source).map_err(|_| error("source contains a NUL byte"))?;
    let target = cstring(target)?;
    let fstype = CString::new(fstype).unwrap();
    let data = data.map(|data| CString::new(data).map_err(|_| error("options contain a NUL byte"))).transpose()?;
    let data_ptr = data.as_ref().map_or(std::ptr::null(), |data| data.as_ptr() as *const libc::c_void);
    check(unsafe { libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), flags, data_ptr) })
}

/// Mount an EROFS image read-only, straight from the file
fn mount_image(image: &File, target: &Path) -> io::Result<()> {
    let flags = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV;
    mount(&fd_path(image), target, "erofs", flags, None).map_err(|e| match e.raw_os_error() {
        // the source is not a block device, and the kernel cannot mount EROFS from a file
        Some(libc::ENOTBLK) => error("the kernel cannot mount EROFS images from a file (needs EROFS_FS_BACKED_BY_FILE)"),
        _ => e,
    })
}

fn unmount_path(path: &Path) -> io::Result<()> {
    let path = cstring(path)?;
    if unsafe { libc::umount2(path.as_ptr(), libc::UMOUNT_NOFOLLOW) } == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        // not mounted
        Some(libc::EINVAL) | Some(libc::ENOENT) => Ok(()),
        // still in use, e.g. by a game process that did not exit: detach it, so the next mount
        // starts clean, and it goes away once the last user is gone
        Some(libc::EBUSY) => check(unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH | libc::UMOUNT_NOFOLLOW) }),
        _ => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_names_round_trip() {
        for slot in Slot::ALL {
            assert_eq!(Slot::from_name(slot.name()), Some(slot));
        }
        assert_eq!(Slot::from_name("../etc"), None);
    }
}

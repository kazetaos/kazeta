// Power button handling for the Kazeta OS session.
// logind is configured there with HandlePowerKey=lock, so a power button press
// shows up as a Lock signal on our session instead of powering off right away.
// The launcher decides what a press means: stop the game, or shut down from the bios.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

/// Set by kazeta-session to enable power button handling; desktop sessions leave the power button alone
const ENABLE_ENV: &str = "KAZETA_HANDLE_POWER_KEY";

#[derive(Clone)]
pub struct PowerKey(Arc<AtomicBool>);

impl PowerKey {
    /// Whether the power button was pressed since the last `clear`
    pub fn pressed(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn clear(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Start watching for power button presses if enabled for this session
pub fn watch() -> PowerKey {
    let flag = Arc::new(AtomicBool::new(false));
    if std::env::var_os(ENABLE_ENV).is_some_and(|v| v == "1") {
        let thread_flag = flag.clone();
        thread::spawn(move || {
            if let Err(e) = watch_lock_signal(&thread_flag) {
                eprintln!("Failed to watch for power button presses: {}", e);
            }
        });
    }
    PowerKey(flag)
}

fn watch_lock_signal(flag: &AtomicBool) -> zbus::Result<()> {
    let conn = Connection::system()?;
    let manager = Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?;
    let session: OwnedObjectPath = manager.call("GetSession", &("auto",))?;
    let session = Proxy::new(&conn, "org.freedesktop.login1", session, "org.freedesktop.login1.Session")?;
    for _ in session.receive_signal("Lock")? {
        eprintln!("Power button pressed");
        flag.store(true, Ordering::Relaxed);
    }
    Ok(())
}

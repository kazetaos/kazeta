// Termination signal handling for the launcher.
// Instead of dying on SIGTERM/SIGINT/SIGHUP, the signal is recorded and
// forwarded to the running child so the normal cleanup path still runs.

use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct Terminated(Arc<AtomicBool>);

impl Terminated {
    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Start catching termination signals; must be called before spawning children
pub fn install() -> Terminated {
    let flag = Arc::new(AtomicBool::new(false));
    match Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        Ok(mut signals) => {
            let thread_flag = flag.clone();
            thread::spawn(move || {
                for sig in signals.forever() {
                    eprintln!("Received signal {}, shutting down", sig);
                    thread_flag.store(true, Ordering::Relaxed);
                }
            });
        }
        Err(e) => eprintln!("Failed to install signal handlers: {}", e),
    }
    Terminated(flag)
}

/// Wait for `child` to exit, passing on SIGTERM once `stop` returns true (e.g. after a
/// termination signal) and killing it if it is still running after `kill_timeout`
pub fn wait_child(child: &mut Child, stop: &dyn Fn() -> bool, kill_timeout: Duration) -> std::io::Result<ExitStatus> {
    let mut forwarded_at: Option<Instant> = None;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }

        // the child has not been reaped yet, so its pid cannot have been reused
        if stop() {
            match forwarded_at {
                None => {
                    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
                    forwarded_at = Some(Instant::now());
                }
                Some(at) if at.elapsed() > kill_timeout => {
                    eprintln!("Child {} did not exit, killing it", child.id());
                    let _ = child.kill();
                }
                _ => {}
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}

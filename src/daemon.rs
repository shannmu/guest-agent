//! Watchdog: re-spawns the worker process after any exit or crash.
//!
//! The watchdog re-executes the **same binary** with `--worker` appended so
//! that the worker inherits all original arguments (e.g. `--interval`).

use anyhow::Result;
use std::time::Duration;

const RESPAWN_DELAY: Duration = Duration::from_millis(100);

/// Runs the watchdog loop.
///
/// Spawns the worker as a child process and immediately waits for it.
/// On any exit (clean, panic, signal) the child is restarted after
/// [`RESPAWN_DELAY`].  This loop never returns under normal operation.
pub fn run_watchdog() -> Result<()> {
    let exe = std::env::current_exe()?;
    // Forward every argument except argv[0]; the worker flag is appended.
    let forward_args: Vec<String> = std::env::args().skip(1).collect();

    loop {
        let mut child = std::process::Command::new(&exe)
            .args(&forward_args)
            .arg("--worker")
            .spawn()?;

        let pid = child.id();
        log::info!("spawned worker (PID {pid})");

        let status = child.wait()?;

        if status.success() {
            log::info!("worker (PID {pid}) exited cleanly — respawning");
        } else {
            log::warn!("worker (PID {pid}) exited with status: {status} — respawning");
        }

        std::thread::sleep(RESPAWN_DELAY);
    }
}

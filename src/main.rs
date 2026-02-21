mod daemon;
mod qos;
mod shared_mem;

use anyhow::Result;
use std::time::Duration;

/// Path exposed by the pvsched_guest kernel module.
const PVSCHED_GUEST_DEV: &str = "/dev/pvsched_guest";

const DEFAULT_QOS_INTERVAL_MS: u64 = 100;

/// Internal flag appended by the watchdog when re-spawning itself as a worker.
const WORKER_FLAG: &str = "--worker";

fn main() -> Result<()> {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    let is_worker = args.iter().any(|a| a == WORKER_FLAG);

    let interval_ms = args
        .iter()
        .position(|a| a == "--interval")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_QOS_INTERVAL_MS);

    let interval = Duration::from_millis(interval_ms);

    if is_worker {
        log::info!("worker started (QoS interval={interval:?})");
        run_worker(interval)
    } else {
        log::info!("watchdog started");
        daemon::run_watchdog()
    }
}

/// Worker entry point: initialises shared memory then drives the QoS loop.
fn run_worker(interval: Duration) -> Result<()> {
    let mut shm = shared_mem::SharedMem::open(PVSCHED_GUEST_DEV)?;
    shm.init_guest_area()?;
    let vcpu_count = shm.vcpu_count();
    log::info!("detected {vcpu_count} vCPU(s)");

    let mut collector = qos::QosCollector::new(shm, interval, vcpu_count);
    collector.run()
}

//! QoS collection: high-precision timer loop that samples per-vCPU deadline
//! pressure and writes the results into shared memory.
//!
//! # Extension point
//!
//! Implement [`PressureSource`] to plug in the real data back-end.
//! The provided [`StubPressureSource`] returns all-zero statistics and is
//! intended only as a compile-time placeholder until the eBPF reader (see
//! `metrics/dl_miss_count/`) is integrated via libbpf-rs or a perf-buffer
//! mechanism.

use crate::shared_mem::{SharedMem, VcpuQosData};
use anyhow::Result;
use std::time::{Duration, Instant};

// ─────────────────────── public data types ───────────────────────────────────

/// Per-vCPU deadline pressure snapshot returned by a [`PressureSource`].
pub struct VcpuStat {
    pub vcpu_id: usize,
    /// Deadline misses observed since the last collection tick.
    pub deadline_miss_count: u64,
    /// Maximum deadline lateness (ns) in the same window; 0 if none.
    pub max_lateness_ns: u64,
}

// ─────────────────────── PressureSource trait ────────────────────────────────

/// Source of per-vCPU QoS metrics.
///
/// Implement this trait to connect the collector to a concrete back-end:
/// - eBPF perf-buffer / ring-buffer reader (see `metrics/dl_miss_count/`)
/// - `/proc/sched_debug` parser
/// - cgroup PSI reader
///
/// The implementation must be `Send` because the collector may run in a
/// dedicated thread.
pub trait PressureSource: Send {
    /// Collect one snapshot of per-vCPU statistics.
    ///
    /// Called once per collection tick.  Implementations should reset their
    /// per-epoch counters after each call so that the next tick reflects only
    /// the activity since the previous call.
    fn collect(&mut self) -> Result<Vec<VcpuStat>>;
}

// ─────────────────────── stub implementation ─────────────────────────────────

/// Zero-pressure stub — replace with a real [`PressureSource`] before
/// deploying.
///
/// Suggested replacement: an eBPF reader that polls the `stats` BPF map
/// produced by `dl_miss_count.bpf.c` and maps per-CPU entries to vCPU IDs.
pub struct StubPressureSource {
    vcpu_count: usize,
}

impl StubPressureSource {
    pub fn new(vcpu_count: usize) -> Self {
        Self { vcpu_count }
    }
}

impl PressureSource for StubPressureSource {
    fn collect(&mut self) -> Result<Vec<VcpuStat>> {
        // TODO: read from the `stats` BPF PERCPU_HASH map populated by
        //       dl_miss_count.bpf.c via a libbpf-rs skeleton or manual fd.
        Ok((0..self.vcpu_count)
            .map(|id| VcpuStat {
                vcpu_id: id,
                deadline_miss_count: 0,
                max_lateness_ns: 0,
            })
            .collect())
    }
}

// ────────────────────────── QosCollector ─────────────────────────────────────

/// Drives the high-precision collection timer and writes data into shared
/// memory.
///
/// Uses `std::thread::sleep` (which wraps `nanosleep(2)`) as the high-
/// precision user-space timer mandated by the design document.  The sleep
/// duration is adjusted each tick to compensate for collection overhead so
/// that the effective interval stays close to the configured value.
pub struct QosCollector {
    shm: SharedMem,
    interval: Duration,
    source: Box<dyn PressureSource>,
}

impl QosCollector {
    pub fn new(shm: SharedMem, interval: Duration, vcpu_count: usize) -> Self {
        Self {
            shm,
            interval,
            source: Box::new(StubPressureSource::new(vcpu_count)),
        }
    }

    /// Replace the default stub source with a custom implementation.
    pub fn with_source(mut self, source: impl PressureSource + 'static) -> Self {
        self.source = Box::new(source);
        self
    }

    /// Run the collection loop.  Blocks indefinitely.
    pub fn run(&mut self) -> Result<()> {
        log::info!("QoS loop running (interval={:?})", self.interval);
        loop {
            let tick_start = Instant::now();

            self.tick()?;

            // Compensate for collection overhead to keep the interval accurate.
            let elapsed = tick_start.elapsed();
            if let Some(remaining) = self.interval.checked_sub(elapsed) {
                std::thread::sleep(remaining);
            } else {
                log::warn!(
                    "collection overran interval by {:?}",
                    elapsed - self.interval
                );
            }
        }
    }

    // ── private ──────────────────────────────────────────────────────────────

    fn tick(&mut self) -> Result<()> {
        let now_ns = monotonic_ns();
        let samples = self.source.collect()?;

        for s in samples {
            self.shm.write_vcpu_qos(
                s.vcpu_id,
                VcpuQosData {
                    timestamp_ns: now_ns,
                    deadline_miss_count: s.deadline_miss_count,
                    max_lateness_ns: s.max_lateness_ns,
                    _reserved: [0; 8],
                },
            );
        }

        Ok(())
    }
}

// ─────────────────────────────── helpers ─────────────────────────────────────

/// Returns `CLOCK_MONOTONIC` in nanoseconds via a direct libc call.
fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is a valid out-pointer; CLOCK_MONOTONIC always succeeds.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

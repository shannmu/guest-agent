//! QoS collection: high-precision timer loop that samples per-vCPU pressure
//! and writes the results into shared memory.
//!
//! # Extension point
//!
//! Implement [`PressureSource`] to plug in the real data back-end.
//! The provided [`StubPressureSource`] returns all-zero statistics and is
//! intended only as a compile-time placeholder until the eBPF reader (see
//! `metrics/dl_miss_count/`) is integrated via libbpf-rs or a perf-buffer
//! mechanism.

use crate::shared_mem::SharedMem;
use anyhow::Result;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ─────────────────────── public data types ───────────────────────────────────

/// Per-vCPU QoS pressure snapshot returned by a [`PressureSource`].
pub struct VcpuStat {
    pub vcpu_id: usize,
    /// Pressure ratio in [0.0, 1.0].
    pub pressure: f64,
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
                pressure: 0.0,
            })
            .collect())
    }
}

// ───────────────────────────── PSI source ───────────────────────────────────

/// PSI-based QoS source backed by one or more cgroup v2 `cpu.pressure` files.
pub struct PsiPressureSource {
    vcpu_count: usize,
    sources: Vec<PsiCgroupSource>,
}

impl PsiPressureSource {
    /// Build a PSI source from one or more cgroup paths (directories or
    /// explicit `cpu.pressure` file paths).
    pub fn try_new(
        cgroup_paths: impl IntoIterator<Item = impl Into<PathBuf>>,
        vcpu_count: usize,
    ) -> Result<Self> {
        let mut sources = Vec::new();
        for path in cgroup_paths {
            let path = path.into();
            match PsiCgroupSource::try_new(path.clone(), vcpu_count) {
                Ok(source) => sources.push(source),
                Err(err) => {
                    log::warn!(
                        "PSI source disabled for {}: {err}",
                        path.display()
                    );
                }
            }
        }
        if sources.is_empty() {
            anyhow::bail!("no valid PSI cgroup paths found");
        }
        Ok(Self { vcpu_count, sources })
    }
}

impl PressureSource for PsiPressureSource {
    fn collect(&mut self) -> Result<Vec<VcpuStat>> {
        let mut per_vcpu: Vec<Option<f64>> = vec![None; self.vcpu_count];
        for source in &mut self.sources {
            let pressure = source.sample_pressure()?;
            for &id in &source.target_vcpus {
                let slot = &mut per_vcpu[id];
                *slot = Some(match *slot {
                    Some(existing) => existing.max(pressure),
                    None => pressure,
                });
            }
        }

        Ok(per_vcpu
            .into_iter()
            .enumerate()
            .filter_map(|(id, pressure)| {
                pressure.map(|pressure| VcpuStat {
                    vcpu_id: id,
                    pressure,
                })
            })
            .collect())
    }
}

struct PsiCgroupSource {
    target_vcpus: Vec<usize>,
    pressure_path: PathBuf,
    last_total_us: Option<u64>,
    last_ts_us: Option<u64>,
}

impl PsiCgroupSource {
    fn try_new(cgroup_path: PathBuf, vcpu_count: usize) -> Result<Self> {
        let (cgroup_dir, pressure_path) = resolve_cgroup_paths(&cgroup_path)?;
        if !pressure_path.exists() {
            anyhow::bail!("PSI cpu.pressure not found at {}", pressure_path.display());
        }

        let cpuset_path = cgroup_dir.join("cpuset.cpus");
        if !cpuset_path.exists() {
            anyhow::bail!("cpuset.cpus not found at {}", cpuset_path.display());
        }

        // Validate the first line can be parsed.
        let _ = read_psi_total_us(&pressure_path)?;

        let cpus_raw = std::fs::read_to_string(&cpuset_path)?;
        let mut target_vcpus = parse_cpuset_cpus(&cpus_raw)?;
        if target_vcpus.is_empty() {
            anyhow::bail!("cpuset.cpus is empty");
        }

        let before = target_vcpus.len();
        target_vcpus.retain(|&cpu| cpu < vcpu_count);
        if target_vcpus.is_empty() {
            anyhow::bail!(
                "cpuset.cpus lists CPUs outside vCPU range (count={vcpu_count})"
            );
        }
        if target_vcpus.len() != before {
            log::warn!(
                "cpuset.cpus has CPUs outside vCPU range (count={vcpu_count}); ignoring them"
            );
        }

        Ok(Self {
            target_vcpus,
            pressure_path,
            last_total_us: None,
            last_ts_us: None,
        })
    }

    fn sample_pressure(&mut self) -> Result<f64> {
        let now_us = monotonic_us();
        let total_us = read_psi_total_us(&self.pressure_path)?;

        let pressure = match (self.last_total_us, self.last_ts_us) {
            (Some(prev_total), Some(prev_ts)) => {
                let delta_t = now_us.saturating_sub(prev_ts);
                let delta_stall = total_us.saturating_sub(prev_total);
                if delta_t == 0 {
                    0.0
                } else {
                    (delta_stall as f64 / delta_t as f64).min(1.0)
                }
            }
            _ => 0.0,
        };

        self.last_total_us = Some(total_us);
        self.last_ts_us = Some(now_us);

        Ok(pressure)
    }
}

// ────────────────────────── QosCollector ─────────────────────────────────────

/// Drives the high-precision collection timer and writes data into shared
/// memory.
///
/// Uses Linux `timerfd` as the high-precision user-space timer mandated by the
/// design document.
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
        let mut timer = TimerFd::new(self.interval)?;
        loop {
            let expirations = timer.wait()?;
            if expirations > 1 {
                log::warn!("collection missed {expirations} ticks");
            }
            self.tick()?;
        }
    }

    // ── private ──────────────────────────────────────────────────────────────

    fn tick(&mut self) -> Result<()> {
        let samples = self.source.collect()?;

        for s in samples {
            self.shm.write_vcpu_pressure(s.vcpu_id, s.pressure);
        }

        Ok(())
    }
}

// ─────────────────────────────── helpers ─────────────────────────────────────

/// Returns `CLOCK_MONOTONIC` in nanoseconds via a direct libc call.
fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid out-pointer; CLOCK_MONOTONIC always succeeds.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn monotonic_us() -> u64 {
    monotonic_ns() / 1_000
}

fn read_psi_total_us(path: &Path) -> Result<u64> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let _ = reader.read_line(&mut line)?;
    parse_psi_total_us(&line)
}

fn parse_psi_total_us(line: &str) -> Result<u64> {
    for token in line.split_whitespace() {
        if let Some(total) = token.strip_prefix("total=") {
            return total
                .parse::<u64>()
                .map_err(|err| anyhow::anyhow!("invalid PSI total value: {err}"));
        }
    }
    anyhow::bail!("PSI total field not found")
}

fn resolve_cgroup_paths(path: &Path) -> Result<(PathBuf, PathBuf)> {
    if let Some(name) = path.file_name() {
        if name == "cpu.pressure" {
            let cgroup_dir = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("cpu.pressure path has no parent"))?
                .to_path_buf();
            return Ok((cgroup_dir, path.to_path_buf()));
        }
    }

    Ok((path.to_path_buf(), path.join("cpu.pressure")))
}

fn parse_cpuset_cpus(content: &str) -> Result<Vec<usize>> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        anyhow::bail!("cpuset.cpus is empty");
    }

    let mut cpus = std::collections::BTreeSet::new();
    for token in trimmed.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Some((start, end)) = token.split_once('-') {
            let start = start
                .trim()
                .parse::<usize>()
                .map_err(|err| anyhow::anyhow!("invalid cpuset cpu '{start}': {err}"))?;
            let end = end
                .trim()
                .parse::<usize>()
                .map_err(|err| anyhow::anyhow!("invalid cpuset cpu '{end}': {err}"))?;
            if start > end {
                anyhow::bail!("invalid cpuset range {start}-{end}");
            }
            for cpu in start..=end {
                cpus.insert(cpu);
            }
        } else {
            let cpu = token
                .parse::<usize>()
                .map_err(|err| anyhow::anyhow!("invalid cpuset cpu '{token}': {err}"))?;
            cpus.insert(cpu);
        }
    }

    Ok(cpus.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::{parse_cpuset_cpus, parse_psi_total_us};

    #[test]
    fn parse_psi_total_us_ok() {
        let line = "some avg10=0.00 avg60=0.00 avg300=0.00 total=123456";
        let total = parse_psi_total_us(line).expect("parse should succeed");
        assert_eq!(total, 123456);
    }

    #[test]
    fn parse_psi_total_us_missing() {
        let line = "some avg10=0.00 avg60=0.00 avg300=0.00";
        let err = parse_psi_total_us(line).unwrap_err().to_string();
        assert!(err.contains("PSI total field not found"));
    }

    #[test]
    fn parse_psi_total_us_invalid() {
        let line = "some avg10=0.00 avg60=0.00 avg300=0.00 total=oops";
        let err = parse_psi_total_us(line).unwrap_err().to_string();
        assert!(err.contains("invalid PSI total value"));
    }

    #[test]
    fn parse_cpuset_cpus_single() {
        let cpus = parse_cpuset_cpus("2").expect("parse should succeed");
        assert_eq!(cpus, vec![2]);
    }

    #[test]
    fn parse_cpuset_cpus_ranges() {
        let cpus = parse_cpuset_cpus("0-2,4,6-7").expect("parse should succeed");
        assert_eq!(cpus, vec![0, 1, 2, 4, 6, 7]);
    }

    #[test]
    fn parse_cpuset_cpus_invalid_range() {
        let err = parse_cpuset_cpus("3-1").unwrap_err().to_string();
        assert!(err.contains("invalid cpuset range"));
    }
}

// ───────────────────────────── timerfd ──────────────────────────────────────

struct TimerFd {
    fd: i32,
    interval: Duration,
    next_deadline_ns: u64,
}

impl TimerFd {
    fn new(interval: Duration) -> Result<Self> {
        if interval.is_zero() {
            anyhow::bail!("interval must be non-zero");
        }
        // SAFETY: timerfd_create returns a new fd or -1 on error.
        let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC) };
        if fd < 0 {
            anyhow::bail!("timerfd_create failed: {}", std::io::Error::last_os_error());
        }

        let interval_ns = duration_to_ns(interval)?;
        let now_ns = monotonic_ns();
        let next_deadline_ns = align_next_deadline(now_ns, interval_ns);

        let timer = Self {
            fd,
            interval,
            next_deadline_ns,
        };
        if let Err(err) = timer.arm_absolute_deadline(next_deadline_ns) {
            // SAFETY: close a valid fd.
            unsafe { libc::close(fd) };
            return Err(err);
        }

        Ok(timer)
    }

    fn wait(&mut self) -> Result<u64> {
        let mut expirations: u64 = 0;
        let mut read_bytes = 0;
        let target = std::mem::size_of::<u64>();
        let buf = &mut expirations as *mut u64 as *mut u8;

        while read_bytes < target {
            // SAFETY: buffer is valid and sized for u64.
            let ptr = unsafe { buf.add(read_bytes) } as *mut libc::c_void;
            let rc = unsafe { libc::read(self.fd, ptr, target - read_bytes) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                anyhow::bail!("timerfd read failed: {err}");
            }
            read_bytes += rc as usize;
        }

        let now_ns = monotonic_ns();
        let interval_ns = duration_to_ns(self.interval)?;
        let mut missed = 0_u64;

        while self.next_deadline_ns <= now_ns {
            self.next_deadline_ns = self.next_deadline_ns.saturating_add(interval_ns);
            missed = missed.saturating_add(1);
        }

        self.arm_absolute_deadline(self.next_deadline_ns)?;

        Ok(missed.max(expirations))
    }

    fn arm_absolute_deadline(&self, deadline_ns: u64) -> Result<()> {
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: ns_to_timespec(deadline_ns),
        };

        // SAFETY: valid fd and itimerspec pointer.
        let rc = unsafe {
            libc::timerfd_settime(
                self.fd,
                libc::TFD_TIMER_ABSTIME,
                &spec,
                std::ptr::null_mut(),
            )
        };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("timerfd_settime failed: {err}");
        }

        Ok(())
    }
}

impl Drop for TimerFd {
    fn drop(&mut self) {
        // SAFETY: fd created by timerfd_create.
        unsafe { libc::close(self.fd) };
    }
}

fn duration_to_ns(duration: Duration) -> Result<u64> {
    let secs = duration.as_secs();
    let nanos = duration.subsec_nanos() as u64;
    let base = secs
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(nanos))
        .ok_or_else(|| anyhow::anyhow!("interval too large to represent in ns"))?;
    Ok(base)
}

fn ns_to_timespec(ns: u64) -> libc::timespec {
    libc::timespec {
        tv_sec: (ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (ns % 1_000_000_000) as libc::c_long,
    }
}

fn align_next_deadline(now_ns: u64, interval_ns: u64) -> u64 {
    let now = now_ns as u128;
    let interval = interval_ns as u128;
    let next = (now / interval + 1) * interval;
    next.min(u128::from(u64::MAX)) as u64
}

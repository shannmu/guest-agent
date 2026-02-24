//! Shared memory interface with the host via `/dev/pvsched_guest`.
//!
//! Layout is defined by `pvsched.h` (struct pvsched_shared_mem).

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Maximum number of vCPUs supported by the ABI.
pub const PVSCHED_MAX_VCPU: usize = 16;

/// Scale factor that maps [0.0, 1.0] to [0, PVSCHED_PRESSURE_SCALE].
pub const PVSCHED_PRESSURE_SCALE: u64 = 1024;

// ─────────────────────────── ABI structs ────────────────────────────────────

#[repr(C)]
pub struct PvschedInfo {
    pub qos_pressure: AtomicU64,
    pub update_seq: AtomicU64,
    pub tokens: AtomicI64,
}

#[repr(C)]
pub struct PvschedSharedMem {
    pub epoch: AtomicU32,
    pub tgid: libc::pid_t,
    pub vcpu_num: u32,
    pub info: [PvschedInfo; 0],
}

// ────────────────────────────── SharedMem ────────────────────────────────────

/// Owned handle to the pvsched_guest shared memory region.
pub struct SharedMem {
    ptr: *mut u8,
    size: usize,
}

// SAFETY: SharedMem is the sole owner of the mmap'd region; it is not cloned.
unsafe impl Send for SharedMem {}

impl SharedMem {
    /// Open the pvsched_guest device and map it into the process address space.
    pub fn open(dev_path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dev_path)
            .with_context(|| format!("failed to open {dev_path}"))?;

        let fd = file.as_raw_fd();
        let size = Self::probe_size();

        // SAFETY: standard mmap call; error is checked below.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            anyhow::bail!(
                "mmap({dev_path}) failed: {}",
                std::io::Error::last_os_error()
            );
        }

        // The file descriptor may be closed after mmap succeeds.
        Ok(Self { ptr: ptr as *mut u8, size })
    }

    /// Validate shared memory metadata written by the host.
    pub fn init_guest_area(&mut self) -> Result<()> {
        let vcpu_count = self.vcpu_count();
        if !(1..=PVSCHED_MAX_VCPU).contains(&vcpu_count) {
            anyhow::bail!("invalid vcpu_num in shared memory: {vcpu_count}");
        }
        Ok(())
    }

    /// Returns the number of vCPUs recorded by the host.
    pub fn vcpu_count(&self) -> usize {
        // SAFETY: mapping starts with pvsched_shared_mem.
        let mem = unsafe { &*(self.ptr as *const PvschedSharedMem) };
        mem.vcpu_num as usize
    }

    /// Write QoS pressure for one vCPU. Silently ignored for out-of-range IDs.
    ///
    /// Update qos_pressure and bump update_seq, as required by the ABI contract.
    pub fn write_vcpu_pressure(&mut self, vcpu_id: usize, pressure: f64) {
        if vcpu_id >= PVSCHED_MAX_VCPU {
            return;
        }
        let mem = unsafe { &*(self.ptr as *const PvschedSharedMem) };
        if vcpu_id >= mem.vcpu_num as usize {
            return;
        }

        let clamped = pressure.clamp(0.0, 1.0);
        let scaled = (clamped * PVSCHED_PRESSURE_SCALE as f64) as u64;
        let info_ptr = unsafe { mem.info.as_ptr().add(vcpu_id) };
        // SAFETY: info_ptr points inside the mapped shared memory region.
        unsafe {
            (*info_ptr).qos_pressure.store(scaled, Ordering::SeqCst);
            (*info_ptr).update_seq.fetch_add(1, Ordering::SeqCst);
        }
    }

    // ── private helpers ───────────────────────────────────────────────────────

    /// Mapping size as defined by the ABI (one 4 KiB page).
    fn probe_size() -> usize {
        4 * 1024
    }
}

impl Drop for SharedMem {
    fn drop(&mut self) {
        // SAFETY: ptr and size were set by a successful mmap call.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.size) };
    }
}

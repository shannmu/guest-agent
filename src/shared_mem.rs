//! Shared memory interface with the host via `/dev/pvsched_guest`.
//!
//! The kernel module `pvsched_guest` exposes the shared region through a
//! `mmap(2)`-able character device.  This module owns the mapping and provides
//! typed accessors for the guest-writable area.
//!
//! # Memory layout
//!
//! ```text
//! ┌─────────────────────────────┐  offset 0
//! │      SharedMemHeader        │  written by host / kernel module (read-only)
//! ├─────────────────────────────┤  offset = header.guest_area_offset
//! │         GuestArea           │  written by this agent
//! │   vcpu_count: u32           │
//! │   vcpu_data[MAX_VCPUS]      │
//! ├─────────────────────────────┤  offset = header.host_area_offset
//! │          HostArea           │  written by host (read-only for guest)
//! └─────────────────────────────┘
//! ```

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::Ordering;

/// Magic number placed by the host in [`SharedMemHeader::magic`].
/// Must match the kernel module definition.
pub const SHARED_MEM_MAGIC: u32 = 0x5053_4348; // 'PSCH'

/// Maximum number of vCPUs supported in the shared area.
pub const MAX_VCPUS: usize = 128;

// ──────────────────────────── memory layout types ────────────────────────────

/// Header written by the host / kernel module.  The guest treats this as
/// read-only after the initial validation in [`SharedMem::init_guest_area`].
#[repr(C)]
pub struct SharedMemHeader {
    /// Must equal [`SHARED_MEM_MAGIC`].
    pub magic: u32,
    pub version: u32,
    /// Byte offset of [`GuestArea`] from the start of the mapping.
    pub guest_area_offset: u32,
    pub guest_area_size: u32,
    /// Byte offset of the host-writable area (read-only for guest).
    pub host_area_offset: u32,
    pub host_area_size: u32,
    pub _reserved: [u8; 40],
}

/// Per-vCPU QoS data written by this agent into the shared area.
///
/// Fields are updated atomically per-vCPU; the host reads them after
/// observing the release fence in [`SharedMem::write_vcpu_qos`].
#[repr(C)]
pub struct VcpuQosData {
    /// CLOCK_MONOTONIC timestamp (ns) at the time of collection.
    pub timestamp_ns: u64,
    /// Deadline miss count aggregated over the last collection epoch.
    /// PSI source uses this field for QoS in fixed-point PPM (1_000_000 == 100% stall).
    pub deadline_miss_count: u64,
    /// Maximum deadline lateness (ns) observed in the last epoch.
    /// Negative values are clamped to 0 by the collector.
    pub max_lateness_ns: u64,
    pub _reserved: [u8; 8],
}

impl Default for VcpuQosData {
    fn default() -> Self {
        // SAFETY: all-zero is valid for this plain-data struct.
        unsafe { std::mem::zeroed() }
    }
}

/// Guest-writable region.
#[repr(C)]
pub struct GuestArea {
    /// Number of vCPUs actually present; set during initialisation.
    pub vcpu_count: u32,
    pub _pad: [u8; 60],
    pub vcpu_data: [VcpuQosData; MAX_VCPUS],
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
        let size = Self::probe_size(fd);

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

    /// Validate the shared-memory header and zero-initialise the guest area.
    ///
    /// Must be called once before any [`write_vcpu_qos`] calls.
    pub fn init_guest_area(&mut self) -> Result<()> {
        let magic = self.header().magic;
        if magic != SHARED_MEM_MAGIC {
            anyhow::bail!(
                "bad shared-memory magic: 0x{magic:08x} (expected 0x{SHARED_MEM_MAGIC:08x})"
            );
        }

        let vcpu_count = detect_vcpu_count();
        let area = self.guest_area_mut();
        area.vcpu_count = vcpu_count as u32;
        for slot in area.vcpu_data.iter_mut() {
            *slot = VcpuQosData::default();
        }

        // Ensure writes are visible to the host before it reads the area.
        std::sync::atomic::fence(Ordering::SeqCst);

        log::info!("guest area initialised (vcpu_count={vcpu_count})");
        Ok(())
    }

    /// Returns the number of vCPUs recorded during initialisation.
    pub fn vcpu_count(&self) -> usize {
        self.guest_area().vcpu_count as usize
    }

    /// Write QoS data for one vCPU.  Silently ignored for out-of-range IDs.
    pub fn write_vcpu_qos(&mut self, vcpu_id: usize, data: VcpuQosData) {
        if vcpu_id >= MAX_VCPUS {
            return;
        }
        self.guest_area_mut().vcpu_data[vcpu_id] = data;
        // Release fence: host must observe all field writes before it reads
        // timestamp_ns as a sequence counter.
        std::sync::atomic::fence(Ordering::Release);
    }

    // ── private helpers ───────────────────────────────────────────────────────

    fn header(&self) -> &SharedMemHeader {
        // SAFETY: the mapping always starts with the header (offset 0).
        unsafe { &*(self.ptr as *const SharedMemHeader) }
    }

    fn guest_area(&self) -> &GuestArea {
        let offset = self.header().guest_area_offset as usize;
        // SAFETY: offset is within the mapping; validated by init_guest_area.
        unsafe { &*(self.ptr.add(offset) as *const GuestArea) }
    }

    fn guest_area_mut(&mut self) -> &mut GuestArea {
        let offset = self.header().guest_area_offset as usize;
        // SAFETY: offset is within the mapping; validated by init_guest_area.
        unsafe { &mut *(self.ptr.add(offset) as *mut GuestArea) }
    }

    /// Determine the mapping size.
    ///
    /// TODO: query via ioctl or a `/sys` attribute once the kernel module ABI
    /// is finalised.  Falls back to a 64 KiB default for now.
    fn probe_size(_fd: i32) -> usize {
        64 * 1024
    }
}

impl Drop for SharedMem {
    fn drop(&mut self) {
        // SAFETY: ptr and size were set by a successful mmap call.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.size) };
    }
}

// ─────────────────────────────── helpers ─────────────────────────────────────

fn detect_vcpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

use anyhow::{anyhow, Result};
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::fs::File;
use std::os::fd::{FromRawFd, RawFd};
use std::path::Path;

pub const SECTOR_SIZE: usize = 4096;

/// Page-aligned memory buffer for DMA and io_uring operations
pub struct AlignedBuffer {
    ptr: *mut u8,
    layout: Layout,
    capacity: usize,
}

impl AlignedBuffer {
    pub fn new(capacity: usize, align: usize) -> Result<Self> {
        let layout = Layout::from_size_align(capacity, align)
            .map_err(|e| anyhow!("Invalid layout alignment: {}", e))?;
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(anyhow!("Failed to allocate aligned memory of size {}", capacity));
        }
        Ok(Self { ptr, layout, capacity })
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.capacity) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.capacity) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { dealloc(self.ptr, self.layout) };
        }
    }
}

/// Standard File writer matching aria2c page-cache model with disk space pre-allocation
pub struct DirectFileWriter {
    raw_fd: RawFd,
    _file: File,
    current_offset: u64,
}

impl DirectFileWriter {
    pub fn create<P: AsRef<Path>>(path: P, content_length: Option<u64>) -> Result<Self> {
        // Standard buffered page cache file write (level field with aria2c)
        let oflags = OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_TRUNC;
        let mode = Mode::S_IRUSR | Mode::S_IWUSR | Mode::S_IRGRP | Mode::S_IROTH;

        let fd = open(path.as_ref(), oflags, mode)
            .map_err(|e| anyhow!("Failed to open file {:?}: {}", path.as_ref(), e))?;

        let _file = unsafe { File::from_raw_fd(fd) };

        if let Some(len) = content_length {
            if len > 0 {
                let res = unsafe { libc::posix_fallocate(fd, 0, len as libc::off_t) };
                if res != 0 {
                    eprintln!("⚠️ Warning: posix_fallocate failed with error code: {}", res);
                } else {
                    println!("📦 Pre-allocated disk space: {} bytes", len);
                }
            }
        }

        Ok(Self {
            raw_fd: fd,
            _file,
            current_offset: 0,
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.raw_fd
    }

    pub fn current_offset(&self) -> u64 {
        self.current_offset
    }

    pub fn advance_offset(&mut self, bytes: u64) {
        self.current_offset += bytes;
    }
}

use anyhow::{anyhow, Result};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::unistd::pipe2;
use std::os::fd::{AsRawFd, OwnedFd};

pub struct KernelPipe {
    pub read_fd: OwnedFd,
    pub write_fd: OwnedFd,
    pub capacity: usize,
}

impl KernelPipe {
    pub fn new() -> Result<Self> {
        let (pipe_r, pipe_w) = pipe2(OFlag::O_CLOEXEC)?;
        let capacity: usize = 16777216; // 16 MiB max pipe capacity
        
        fcntl(
            pipe_r.as_raw_fd(),
            FcntlArg::F_SETPIPE_SZ(capacity as libc::c_int),
        ).map_err(|e| anyhow!("Failed to set 16 MiB pipe capacity. This is required for WAN performance. Check /proc/sys/fs/pipe-max-size: {}", e))?;

        Ok(Self {
            read_fd: pipe_r,
            write_fd: pipe_w,
            capacity,
        })
    }
}



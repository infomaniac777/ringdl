use anyhow::Result;
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
        let mut capacity: usize = 1048576; // 1 MiB max pipe capacity
        if fcntl(
            pipe_r.as_raw_fd(),
            FcntlArg::F_SETPIPE_SZ(capacity as libc::c_int),
        )
        .is_err()
        {
            capacity = 262144; // Fallback to 256 KiB
            let _ = fcntl(
                pipe_r.as_raw_fd(),
                FcntlArg::F_SETPIPE_SZ(capacity as libc::c_int),
            );
        }
        Ok(Self {
            read_fd: pipe_r,
            write_fd: pipe_w,
            capacity,
        })
    }
}



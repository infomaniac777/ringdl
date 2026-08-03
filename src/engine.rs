use anyhow::{anyhow, Result};
use io_uring::{opcode, types, IoUring};
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, SockaddrIn};
use std::net::ToSocketAddrs;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU16, Ordering};

use crate::http::{parse_http_response_header, ParsedUrl};
use crate::storage::{AlignedBuffer, DirectFileWriter, SECTOR_SIZE};

const BUF_BGID: u16 = 1;
const IORING_CQE_F_BUFFER: u32 = 1 << 0;
const IORING_CQE_BUFFER_SHIFT: u32 = 16;

pub struct DownloadEngine {
    ring: IoUring,
    buf_size: usize,
    ring_entries: u16,
    block_size: usize,
}

impl DownloadEngine {
    pub fn new(ring_entries: u16, buf_size: usize, block_size_kb: usize) -> Result<Self> {
        let ring = IoUring::builder()
            .setup_cqsize(ring_entries as u32 * 4)
            .build(ring_entries as u32 * 2)?;

        let block_size = block_size_kb * 1024;
        let block_size = (block_size + SECTOR_SIZE - 1) & !(SECTOR_SIZE - 1); // Align block size to 4096 bytes

        Ok(Self {
            ring,
            buf_size,
            ring_entries,
            block_size,
        })
    }

    pub fn download(&mut self, url: &ParsedUrl, output_path: &Path) -> Result<()> {
        // 1. Resolve host and connect TCP socket
        let addr_str = format!("{}:{}", url.host, url.port);
        let socket_addr = addr_str
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow!("Failed to resolve IP address for host: {}", url.host))?;

        let sock_fd = socket(
            AddressFamily::Inet,
            SockType::Stream,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
            None,
        )?;

        let sockaddr_in = match socket_addr {
            std::net::SocketAddr::V4(v4) => SockaddrIn::from(v4),
            _ => return Err(anyhow!("IPv6 not supported in initial MVP")),
        };

        println!("📡 Connecting to {} ({:?})...", url.host, socket_addr);
        match connect(sock_fd.as_raw_fd(), &sockaddr_in) {
            Ok(_) => {},
            Err(nix::errno::Errno::EINPROGRESS) => {},
            Err(e) => return Err(anyhow!("Socket connection failed: {}", e)),
        }

        // 2. Setup Provided Buffer Ring (io_uring_buf_ring)
        let total_buf_bytes = self.buf_size * self.ring_entries as usize;
        let mut raw_buf_pool = AlignedBuffer::new(total_buf_bytes, SECTOR_SIZE)?;
        
        // Setup ring buffer memory in user-space
        let buf_ring_layout = std::alloc::Layout::from_size_align(
            self.ring_entries as usize * std::mem::size_of::<types::BufRingEntry>(),
            4096,
        ).map_err(|e| anyhow!("Layout error: {}", e))?;
        
        let buf_ring_ptr = unsafe { std::alloc::alloc_zeroed(buf_ring_layout) as *mut types::BufRingEntry };
        if buf_ring_ptr.is_null() {
            return Err(anyhow!("Failed to allocate memory for BufRingEntry ring"));
        }

        let submitter = self.ring.submitter();
        // Register buf ring with kernel
        unsafe {
            submitter.register_buf_ring_with_flags(buf_ring_ptr as u64, self.ring_entries, BUF_BGID, 0)?;
        }

        // Fill initial provided buffer ring entries
        let buf_ring_slice = unsafe {
            std::slice::from_raw_parts_mut(buf_ring_ptr, self.ring_entries as usize)
        };

        let raw_slice = raw_buf_pool.as_mut_slice();
        for i in 0..self.ring_entries {
            let buf_offset = i as usize * self.buf_size;
            let buf_addr = unsafe { raw_slice.as_mut_ptr().add(buf_offset) };
            buf_ring_slice[i as usize].set_addr(buf_addr as u64);
            buf_ring_slice[i as usize].set_len(self.buf_size as u32);
            buf_ring_slice[i as usize].set_bid(i);
        }

        // Update tail register for kernel visibility
        let tail_ptr = unsafe { types::BufRingEntry::tail(buf_ring_ptr) as *const AtomicU16 };
        unsafe {
            (*tail_ptr).store(self.ring_entries, Ordering::Release);
        }

        // 3. Send HTTP GET Request
        let http_req = url.build_get_request();
        let send_sqe = opcode::Write::new(
            types::Fd(sock_fd.as_raw_fd()),
            http_req.as_ptr(),
            http_req.len() as u32,
        )
        .build()
        .user_data(0x01);

        unsafe {
            self.ring.submission().push(&send_sqe).map_err(|e| anyhow!("SQ full: {}", e))?;
        }
        self.ring.submit_and_wait(1)?;
        
        let send_cqe = self.ring.completion().next().ok_or_else(|| anyhow!("No CQE received for HTTP send"))?;
        if send_cqe.result() < 0 {
            return Err(anyhow!("Failed to send HTTP GET request: errno {}", -send_cqe.result()));
        }

        println!("✅ HTTP GET request sent. Arming multishot io_uring receive engine...");

        // 4. Arm Multishot RECV SQE (1 syscall to arm persistent multishot receive)
        self.arm_recv_multishot(sock_fd.as_raw_fd())?;

        // 5. Active Receive & Direct I/O Write Loop
        let mut header_parsed = false;
        let mut header_accumulator: Vec<u8> = Vec::with_capacity(8192);
        let mut writer: Option<DirectFileWriter> = None;
        let mut total_downloaded_bytes: u64 = 0;
        let mut expected_content_length: Option<u64> = None;

        // Block-aligned staging buffer for O_DIRECT writing
        let mut staging_buffer = AlignedBuffer::new(self.block_size, SECTOR_SIZE)?;
        let mut staging_fill: usize = 0;

        let tail_atomic = unsafe { &*tail_ptr };

        println!("⚡ Receiving payload via provided buffer ring...");

        loop {
            self.ring.submit_and_wait(1)?;
            
            let cqes: Vec<(u64, i32, u32)> = self.ring
                .completion()
                .map(|cqe| (cqe.user_data(), cqe.result(), cqe.flags()))
                .collect();

            for (user_data, res, flags) in cqes {
                if user_data == 0x02 { // Multishot RECV completion
                    if res <= 0 {
                        if res == -libc::ENOBUFS {
                            // Ring buffer empty; re-arm multishot receive
                            self.arm_recv_multishot(sock_fd.as_raw_fd())?;
                            continue;
                        }

                        if res == 0 {
                            println!("\nEOF reached (connection closed by server).");
                        } else {
                            eprintln!("\nRECV completion ended with result: {}", res);
                        }

                        // Flush remaining staging buffer to disk before exit
                        if let Some(ref mut w) = writer {
                            if staging_fill > 0 {
                                self.flush_staging_buffer(w, &staging_buffer, staging_fill)?;
                            }
                        }
                        return Ok(());
                    }

                    let bytes_read = res as usize;

                    // Check if buffer ID was provided by io_uring
                    if flags & IORING_CQE_F_BUFFER != 0 {
                        let bid = (flags >> IORING_CQE_BUFFER_SHIFT) as u16;
                        let buf_offset = bid as usize * self.buf_size;
                        let recv_buf_slice = &raw_buf_pool.as_slice()[buf_offset..buf_offset + bytes_read];

                        if !header_parsed {
                            header_accumulator.extend_from_slice(recv_buf_slice);
                            if let Some(header) = parse_http_response_header(&header_accumulator)? {
                                header_parsed = true;
                                expected_content_length = header.content_length;
                                println!(
                                    "✨ HTTP Header Received! Status: {}, Content-Length: {:?}",
                                    header.status_code,
                                    header.content_length.unwrap_or(0)
                                );

                                // Create O_DIRECT file writer & pre-allocate space
                                let w = DirectFileWriter::create(output_path, header.content_length)?;
                                writer = Some(w);

                                // Payload bytes present after HTTP header boundary (\r\n\r\n)
                                let payload_start = header.header_len;
                                let initial_payload = &header_accumulator[payload_start..];
                                if !initial_payload.is_empty() {
                                    total_downloaded_bytes += initial_payload.len() as u64;
                                    staging_fill = self.append_to_staging(
                                        writer.as_mut().unwrap(),
                                        &mut staging_buffer,
                                        staging_fill,
                                        initial_payload,
                                    )?;
                                }
                            }
                        } else {
                            total_downloaded_bytes += bytes_read as u64;
                            staging_fill = self.append_to_staging(
                                writer.as_mut().unwrap(),
                                &mut staging_buffer,
                                staging_fill,
                                recv_buf_slice,
                            )?;
                        }

                        // Return buffer back to kernel io_uring_buf_ring
                        let buf_addr = unsafe { raw_buf_pool.as_mut_slice().as_mut_ptr().add(buf_offset) };
                        buf_ring_slice[bid as usize].set_addr(buf_addr as u64);
                        buf_ring_slice[bid as usize].set_len(self.buf_size as u32);
                        buf_ring_slice[bid as usize].set_bid(bid);
                        tail_atomic.fetch_add(1, Ordering::Release);
                    }

                    // Print progress
                    if let Some(total) = expected_content_length {
                        let pct = (total_downloaded_bytes as f64 / total as f64) * 100.0;
                        print!("\rProgress: {} / {} bytes ({:.2}%)", total_downloaded_bytes, total, pct);
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                        if total_downloaded_bytes >= total {
                            println!("\n🎉 Download complete!");
                            if let Some(ref mut w) = writer {
                                if staging_fill > 0 {
                                    self.flush_staging_buffer(w, &staging_buffer, staging_fill)?;
                                }
                            }
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    fn arm_recv_multishot(&mut self, sock_fd: i32) -> Result<()> {
        let recv_sqe = opcode::RecvMulti::new(types::Fd(sock_fd), BUF_BGID)
            .build()
            .user_data(0x02);

        unsafe {
            self.ring.submission().push(&recv_sqe).map_err(|e| anyhow!("SQ full: {}", e))?;
        }
        self.ring.submit()?;
        Ok(())
    }

    fn append_to_staging(
        &mut self,
        writer: &mut DirectFileWriter,
        staging_buffer: &mut AlignedBuffer,
        mut current_fill: usize,
        data: &[u8],
    ) -> Result<usize> {
        let mut offset = 0;
        while offset < data.len() {
            let space_left = self.block_size - current_fill;
            let to_copy = std::cmp::min(space_left, data.len() - offset);

            staging_buffer.as_mut_slice()[current_fill..current_fill + to_copy]
                .copy_from_slice(&data[offset..offset + to_copy]);

            current_fill += to_copy;
            offset += to_copy;

            if current_fill == self.block_size {
                self.flush_staging_buffer(writer, staging_buffer, self.block_size)?;
                current_fill = 0;
            }
        }
        Ok(current_fill)
    }

    fn flush_staging_buffer(
        &mut self,
        writer: &mut DirectFileWriter,
        staging_buffer: &AlignedBuffer,
        fill_size: usize,
    ) -> Result<()> {
        // Pad fill size up to sector boundary for O_DIRECT write
        let write_len = (fill_size + SECTOR_SIZE - 1) & !(SECTOR_SIZE - 1);

        // Perform O_DIRECT synchronous pwrite from page-aligned buffer directly to disk
        let written = unsafe {
            libc::pwrite(
                writer.raw_fd(),
                staging_buffer.as_slice().as_ptr() as *const libc::c_void,
                write_len,
                writer.current_offset() as libc::off_t,
            )
        };

        if written < 0 {
            return Err(anyhow!("O_DIRECT pwrite failed: errno {}", std::io::Error::last_os_error()));
        }

        let written = written as usize;
        if written < write_len {
            return Err(anyhow!("Short O_DIRECT write: wrote {} of {} bytes", written, write_len));
        }

        writer.advance_offset(fill_size as u64);

        // Truncate file to exact byte length if final write was padded
        if fill_size < write_len {
            let _ = unsafe { libc::ftruncate(writer.raw_fd(), writer.current_offset() as libc::off_t) };
        }

        Ok(())
    }
}

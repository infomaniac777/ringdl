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

const TAG_HTTP_SEND: u64 = 0x01;
const TAG_RECV_MULTI: u64 = 0x02;
const TAG_DISK_WRITE_BASE: u64 = 0x1000;

pub struct DownloadEngine {
    ring: IoUring,
    buf_size: usize,
    ring_entries: u16,
}

impl DownloadEngine {
    pub fn new(ring_entries: u16, buf_size: usize, _block_size_kb: usize) -> Result<Self> {
        let ring = IoUring::builder()
            .setup_cqsize(ring_entries as u32 * 4)
            .build(ring_entries as u32 * 2)?;

        let buf_size = (buf_size + SECTOR_SIZE - 1) & !(SECTOR_SIZE - 1);

        Ok(Self {
            ring,
            buf_size,
            ring_entries,
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
        
        let buf_ring_layout = std::alloc::Layout::from_size_align(
            self.ring_entries as usize * std::mem::size_of::<types::BufRingEntry>(),
            4096,
        ).map_err(|e| anyhow!("Layout error: {}", e))?;
        
        let buf_ring_ptr = unsafe { std::alloc::alloc_zeroed(buf_ring_layout) as *mut types::BufRingEntry };
        if buf_ring_ptr.is_null() {
            return Err(anyhow!("Failed to allocate memory for BufRingEntry ring"));
        }

        let submitter = self.ring.submitter();
        unsafe {
            submitter.register_buf_ring_with_flags(buf_ring_ptr as u64, self.ring_entries, BUF_BGID, 0)?;
        }

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

        let tail_ptr = unsafe { types::BufRingEntry::tail(buf_ring_ptr) as *const AtomicU16 };
        unsafe {
            (*tail_ptr).store(self.ring_entries, Ordering::Release);
        }

        // 3. Send HTTP GET Request via io_uring
        let http_req = url.build_get_request();
        let send_sqe = opcode::Write::new(
            types::Fd(sock_fd.as_raw_fd()),
            http_req.as_ptr(),
            http_req.len() as u32,
        )
        .build()
        .user_data(TAG_HTTP_SEND);

        unsafe {
            self.ring.submission().push(&send_sqe).map_err(|e| anyhow!("SQ full: {}", e))?;
        }
        self.ring.submit_and_wait(1)?;

        let send_cqe = self.ring.completion().next().ok_or_else(|| anyhow!("No CQE received for HTTP send"))?;
        if send_cqe.result() < 0 {
            return Err(anyhow!("Failed to send HTTP GET request: errno {}", -send_cqe.result()));
        }

        println!("✅ HTTP GET request sent. Arming multishot io_uring receive engine...");

        // 4. Arm Multishot RECV SQE
        self.arm_recv_multishot(sock_fd.as_raw_fd())?;

        let mut header_parsed = false;
        let mut header_accumulator: Vec<u8> = Vec::with_capacity(8192);
        let mut writer: Option<DirectFileWriter> = None;
        let mut total_downloaded_bytes: u64 = 0;
        let mut expected_content_length: Option<u64> = None;
        let mut in_flight_disk_writes: usize = 0;

        let tail_atomic = unsafe { &*tail_ptr };

        println!("⚡ Zero-Copy io_uring Direct Pipeline Active (Network RECV -> Storage WRITE SQEs)...");

        loop {
            if self.ring.completion().is_empty() {
                self.ring.submit_and_wait(1)?;
            }

            let cqes: Vec<(u64, i32, u32)> = self.ring
                .completion()
                .map(|cqe| (cqe.user_data(), cqe.result(), cqe.flags()))
                .collect();

            let mut need_submit = false;
            for (user_data, res, flags) in cqes {
                if user_data == TAG_RECV_MULTI {
                    if res <= 0 {
                        if res == -libc::ENOBUFS {
                            self.arm_recv_multishot(sock_fd.as_raw_fd())?;
                            continue;
                        }

                        if res == 0 {
                            println!("\nEOF reached (connection closed by server).");
                        } else {
                            eprintln!("\nRECV completion ended with result: {}", res);
                        }

                        // Drain remaining in-flight disk writes
                        while in_flight_disk_writes > 0 {
                            self.ring.submit_and_wait(1)?;
                            for cqe in self.ring.completion() {
                                let tag = cqe.user_data();
                                if tag >= TAG_DISK_WRITE_BASE {
                                    in_flight_disk_writes -= 1;
                                }
                            }
                        }
                        return Ok(());
                    }

                    let bytes_read = res as usize;

                    if flags & IORING_CQE_F_BUFFER != 0 {
                        let bid = (flags >> IORING_CQE_BUFFER_SHIFT) as u16;
                        let buf_offset = bid as usize * self.buf_size;
                        let buf_ptr = unsafe { raw_buf_pool.as_mut_slice().as_mut_ptr().add(buf_offset) };

                        if !header_parsed {
                            let recv_buf_slice = unsafe { std::slice::from_raw_parts(buf_ptr, bytes_read) };
                            header_accumulator.extend_from_slice(recv_buf_slice);

                            if let Some(header) = parse_http_response_header(&header_accumulator)? {
                                header_parsed = true;
                                expected_content_length = header.content_length;
                                println!(
                                    "✨ HTTP Header Received! Status: {}, Content-Length: {:?}",
                                    header.status_code,
                                    header.content_length.unwrap_or(0)
                                );

                                let w = DirectFileWriter::create(output_path, header.content_length)?;
                                writer = Some(w);

                                let payload_start = header.header_len;
                                let initial_payload = &header_accumulator[payload_start..];
                                if !initial_payload.is_empty() {
                                    total_downloaded_bytes += initial_payload.len() as u64;
                                    let payload_ptr = unsafe { buf_ptr.add(payload_start) };
                                    
                                    self.submit_direct_storage_write(
                                        writer.as_mut().unwrap(),
                                        payload_ptr,
                                        initial_payload.len(),
                                        bid,
                                    )?;
                                    in_flight_disk_writes += 1;
                                    need_submit = true;
                                } else {
                                    // Return header buffer back to kernel
                                    buf_ring_slice[bid as usize].set_addr(buf_ptr as u64);
                                    buf_ring_slice[bid as usize].set_len(self.buf_size as u32);
                                    buf_ring_slice[bid as usize].set_bid(bid);
                                    tail_atomic.fetch_add(1, Ordering::Release);
                                }
                            } else {
                                // Return header accumulator buffer back to kernel
                                buf_ring_slice[bid as usize].set_addr(buf_ptr as u64);
                                buf_ring_slice[bid as usize].set_len(self.buf_size as u32);
                                buf_ring_slice[bid as usize].set_bid(bid);
                                tail_atomic.fetch_add(1, Ordering::Release);
                            }
                        } else {
                            total_downloaded_bytes += bytes_read as u64;
                            
                            // Submit zero-copy storage write SQE directly from network buffer pointer
                            self.submit_direct_storage_write(
                                writer.as_mut().unwrap(),
                                buf_ptr,
                                bytes_read,
                                bid,
                            )?;
                            in_flight_disk_writes += 1;
                            need_submit = true;
                        }

                        // Progress reporting
                        if let Some(total) = expected_content_length {
                            let pct = (total_downloaded_bytes as f64 / total as f64) * 100.0;
                            print!("\rProgress: {} / {} bytes ({:.2}%)", total_downloaded_bytes, total, pct);
                            use std::io::Write;
                            let _ = std::io::stdout().flush();

                            if total_downloaded_bytes >= total {
                                println!("\n🎉 Download complete!");
                                if need_submit {
                                    self.ring.submit()?;
                                }
                                // Drain remaining disk write SQEs
                                while in_flight_disk_writes > 0 {
                                    self.ring.submit_and_wait(1)?;
                                    for cqe in self.ring.completion() {
                                        let tag = cqe.user_data();
                                        if tag >= TAG_DISK_WRITE_BASE {
                                            in_flight_disk_writes -= 1;
                                        }
                                    }
                                }
                                return Ok(());
                            }
                        }
                    }
                } else if user_data >= TAG_DISK_WRITE_BASE {
                    let bid = (user_data - TAG_DISK_WRITE_BASE) as u16;
                    if res < 0 {
                        return Err(anyhow!("io_uring storage write failed for buf bid {} with errno {}", bid, -res));
                    }
                    if in_flight_disk_writes > 0 {
                        in_flight_disk_writes -= 1;
                    }

                    // Recycle buffer back to io_uring_buf_ring now that storage DMA write is complete!
                    let buf_offset = bid as usize * self.buf_size;
                    let buf_ptr = unsafe { raw_buf_pool.as_mut_slice().as_mut_ptr().add(buf_offset) };
                    buf_ring_slice[bid as usize].set_addr(buf_ptr as u64);
                    buf_ring_slice[bid as usize].set_len(self.buf_size as u32);
                    buf_ring_slice[bid as usize].set_bid(bid);
                    tail_atomic.fetch_add(1, Ordering::Release);
                }
            }

            if need_submit {
                self.ring.submit()?;
            }
        }
    }

    fn arm_recv_multishot(&mut self, sock_fd: i32) -> Result<()> {
        let recv_sqe = opcode::RecvMulti::new(types::Fd(sock_fd), BUF_BGID)
            .build()
            .user_data(TAG_RECV_MULTI);

        unsafe {
            self.ring.submission().push(&recv_sqe).map_err(|e| anyhow!("SQ full: {}", e))?;
        }
        self.ring.submit()?;
        Ok(())
    }

    fn submit_direct_storage_write(
        &mut self,
        writer: &mut DirectFileWriter,
        buf_ptr: *const u8,
        fill_size: usize,
        bid: u16,
    ) -> Result<()> {
        let write_sqe = opcode::Write::new(
            types::Fd(writer.raw_fd()),
            buf_ptr,
            fill_size as u32,
        )
        .offset(writer.current_offset())
        .build()
        .user_data(TAG_DISK_WRITE_BASE + bid as u64);

        unsafe {
            self.ring.submission().push(&write_sqe).map_err(|e| anyhow!("SQ full on write: {}", e))?;
        }

        writer.advance_offset(fill_size as u64);
        Ok(())
    }
}

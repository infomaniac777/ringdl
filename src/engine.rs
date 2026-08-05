use anyhow::{anyhow, Result};
use io_uring::{opcode, types, IoUring};
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, SockaddrIn};
use std::io::Write;
use std::net::ToSocketAddrs;
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;
use rustls::{ClientConfig, RootCertStore};
use std::path::Path;

use crate::http::{parse_http_response_header, ParsedUrl};
use crate::storage::DirectFileWriter;

const TAG_HTTP_SEND: u64 = 0x01;
const TAG_SPLICE_IN: u64 = 0x03;
const TAG_SPLICE_OUT: u64 = 0x04;

pub struct DownloadEngine {
    ring: IoUring,
    buf_size: usize,
}

impl DownloadEngine {
    pub fn new(ring_entries: u16, buf_size: usize, _block_size_kb: usize) -> Result<Self> {
        let ring = IoUring::builder()
            .setup_cqsize(ring_entries as u32 * 4)
            .build(ring_entries as u32 * 2)?;

        Ok(Self {
            ring,
            buf_size,
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
            SockFlag::SOCK_CLOEXEC,
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

        if url.scheme == "https" {
            println!("🔒 Performing TLS Handshake...");
            let mut root_store = RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = root_store.add(cert);
            }
            let mut config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
                .with_root_certificates(root_store)
                .with_no_client_auth();
            config.enable_secret_extraction = true;
            let config = Arc::new(config);

            let server_name = rustls::pki_types::ServerName::try_from(url.host.clone())
                .map_err(|e| anyhow!("Invalid DNS name for TLS: {:?}", e))?
                .to_owned();

            let std_stream = unsafe { std::net::TcpStream::from_raw_fd(sock_fd.as_raw_fd()) };
            std_stream.set_nonblocking(true)?;

            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            
            let ktls_stream = rt.block_on(async {
                let tokio_stream = tokio::net::TcpStream::from_std(std_stream).map_err(|e| anyhow!("from_std failed: {}", e))?;
                let corked_stream = ktls::CorkStream::new(tokio_stream);
                let connector = tokio_rustls::TlsConnector::from(config);
                let tls_stream = connector.connect(server_name, corked_stream).await.map_err(|e| anyhow!("TLS connect failed: {}", e))?;
                println!("🚀 TLS Handshake complete. Offloading to Kernel TLS (kTLS)...");
                ktls::config_ktls_client(tls_stream).await.map_err(|e| anyhow!("kTLS setup failed: {:?}", e))
            })?;
            
            // We leak the ktls_stream to prevent Tokio from running Drop and closing the FD
            std::mem::forget(ktls_stream);
            std::mem::forget(rt);
            println!("✅ kTLS successfully enabled. Socket is now transparently decrypted.");
        }

        // 2. Send HTTP GET Request via io_uring
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

        println!("✅ HTTP GET request sent. Reading HTTP response header...");

        // 3. Read HTTP Response Header
        let mut header_buf = vec![0u8; 8192];
        let mut header_bytes_read = 0;
        let mut expected_content_length: Option<u64> = None;
        let mut writer: Option<DirectFileWriter> = None;
        let mut total_downloaded_bytes: u64 = 0;

        loop {
            let read_sqe = opcode::Read::new(
                types::Fd(sock_fd.as_raw_fd()),
                unsafe { header_buf.as_mut_ptr().add(header_bytes_read) },
                (header_buf.len() - header_bytes_read) as u32,
            )
            .build()
            .user_data(0x10);

            unsafe {
                self.ring.submission().push(&read_sqe).map_err(|e| anyhow!("SQ full on header recv: {}", e))?;
            }
            self.ring.submit_and_wait(1)?;

            let cqe = self.ring.completion().next().ok_or_else(|| anyhow!("No CQE for header recv"))?;
            let res = cqe.result();
            if res <= 0 {
                return Err(anyhow!("Connection closed or error while reading HTTP headers: res {}", res));
            }

            header_bytes_read += res as usize;

            if let Some(header) = parse_http_response_header(&header_buf[..header_bytes_read])? {
                println!(
                    "✨ HTTP Header Received! Status: {}, Content-Length: {:?}",
                    header.status_code,
                    header.content_length.unwrap_or(0)
                );
                expected_content_length = header.content_length;
                let header_len = header.header_len;

                let mut w = DirectFileWriter::create(output_path, header.content_length)?;

                let initial_payload = &header_buf[header_len..header_bytes_read];
                if !initial_payload.is_empty() {
                    let write_sqe = opcode::Write::new(
                        types::Fd(w.raw_fd()),
                        initial_payload.as_ptr(),
                        initial_payload.len() as u32,
                    )
                    .offset(w.current_offset())
                    .build()
                    .user_data(0x20);

                    unsafe {
                        self.ring.submission().push(&write_sqe).map_err(|e| anyhow!("SQ full on initial write: {}", e))?;
                    }
                    self.ring.submit_and_wait(1)?;

                    let w_cqe = self.ring.completion().next().ok_or_else(|| anyhow!("No CQE for initial payload write"))?;
                    if w_cqe.result() < 0 {
                        return Err(anyhow!("Failed to write initial payload: errno {}", -w_cqe.result()));
                    }
                    w.advance_offset(initial_payload.len() as u64);
                    total_downloaded_bytes += initial_payload.len() as u64;
                }

                writer = Some(w);
                break;
            }

            if header_bytes_read == header_buf.len() {
                header_buf.resize(header_buf.len() * 2, 0);
            }
        }

        // 4. In-Kernel Zero-Copy (IORING_OP_SPLICE) Loop: TCP Socket -> Kernel Pipe -> File
        let (pipe_r, pipe_w) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
        let mut pipe_capacity: usize = 1048576; // 1 MiB max pipe capacity
        if nix::fcntl::fcntl(pipe_r.as_raw_fd(), nix::fcntl::FcntlArg::F_SETPIPE_SZ(pipe_capacity as libc::c_int)).is_err() {
            pipe_capacity = 262144;
            let _ = nix::fcntl::fcntl(pipe_r.as_raw_fd(), nix::fcntl::FcntlArg::F_SETPIPE_SZ(pipe_capacity as libc::c_int));
        }

        let splice_chunk = std::cmp::min(self.buf_size, pipe_capacity);

        println!("⚡ Pure In-Kernel Zero-Copy (IORING_OP_SPLICE) Active: TCP Socket -> Kernel Pipe -> Filesystem...");

        if let Some(total) = expected_content_length {
            if total_downloaded_bytes >= total {
                println!("\n🎉 Download complete!");
                return Ok(());
            }
        }

        let remaining_init = match expected_content_length {
            Some(total) => (total - total_downloaded_bytes) as usize,
            None => splice_chunk,
        };
        let first_len = std::cmp::min(splice_chunk, remaining_init);
        self.submit_splice_in(sock_fd.as_raw_fd(), pipe_w.as_raw_fd(), first_len as u32)?;

        let mut current_pipe_bytes: u32 = 0;

        loop {
            self.ring.submit_and_wait(1)?;

            let cqes: Vec<(u64, i32, u32)> = self.ring
                .completion()
                .map(|cqe| (cqe.user_data(), cqe.result(), cqe.flags()))
                .collect();

            for (user_data, res, _) in cqes {
                if user_data == TAG_SPLICE_IN {
                    if res <= 0 {
                        if res == -libc::EAGAIN || res == -libc::EWOULDBLOCK || res == -libc::EINTR {
                            let remaining = match expected_content_length {
                                Some(total) => (total - total_downloaded_bytes) as usize,
                                None => splice_chunk,
                            };
                            let retry_len = std::cmp::min(splice_chunk, remaining);
                            self.submit_splice_in(sock_fd.as_raw_fd(), pipe_w.as_raw_fd(), retry_len as u32)?;
                            continue;
                        }
                        if res == 0 {
                            println!("\nEOF reached on SPLICE_IN.");
                            return Ok(());
                        }
                        return Err(anyhow!("IORING_OP_SPLICE socket -> pipe failed: errno {}", -res));
                    }
                    current_pipe_bytes = res as u32;
                    let w = writer.as_ref().unwrap();
                    self.submit_splice_out(
                        pipe_r.as_raw_fd(),
                        w.raw_fd(),
                        w.current_offset(),
                        current_pipe_bytes,
                    )?;
                } else if user_data == TAG_SPLICE_OUT {
                    if res <= 0 {
                        if res == -libc::EAGAIN || res == -libc::EWOULDBLOCK || res == -libc::EINTR {
                            let w = writer.as_ref().unwrap();
                            self.submit_splice_out(
                                pipe_r.as_raw_fd(),
                                w.raw_fd(),
                                w.current_offset(),
                                current_pipe_bytes,
                            )?;
                            continue;
                        }
                        return Err(anyhow!("IORING_OP_SPLICE pipe -> file failed: errno {}", -res));
                    }
                    let bytes_written = res as u64;
                    let w = writer.as_mut().unwrap();
                    w.advance_offset(bytes_written);
                    total_downloaded_bytes += bytes_written;

                    if let Some(total) = expected_content_length {
                        let pct = (total_downloaded_bytes as f64 / total as f64) * 100.0;
                        print!("\rProgress: {} / {} bytes ({:.2}%)", total_downloaded_bytes, total, pct);
                        let _ = std::io::stdout().flush();

                        if total_downloaded_bytes >= total {
                            println!("\n🎉 Download complete via In-Kernel Zero-Copy (splice)!");
                            return Ok(());
                        }
                    }

                    let remaining = match expected_content_length {
                        Some(total) => (total - total_downloaded_bytes) as usize,
                        None => splice_chunk,
                    };
                    let next_chunk = std::cmp::min(splice_chunk, remaining);
                    if next_chunk > 0 {
                        self.submit_splice_in(sock_fd.as_raw_fd(), pipe_w.as_raw_fd(), next_chunk as u32)?;
                    } else {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn submit_splice_in(&mut self, fd_in: i32, fd_out: i32, len: u32) -> Result<()> {
        let sqe = opcode::Splice::new(
            types::Fd(fd_in),
            -1,
            types::Fd(fd_out),
            -1,
            len,
        )
        .build()
        .user_data(TAG_SPLICE_IN);

        unsafe {
            self.ring.submission().push(&sqe).map_err(|e| anyhow!("SQ full on splice in: {}", e))?;
        }
        self.ring.submit()?;
        Ok(())
    }

    fn submit_splice_out(&mut self, fd_in: i32, fd_out: i32, off_out: u64, len: u32) -> Result<()> {
        let sqe = opcode::Splice::new(
            types::Fd(fd_in),
            -1,
            types::Fd(fd_out),
            off_out as i64,
            len,
        )
        .build()
        .user_data(TAG_SPLICE_OUT);

        unsafe {
            self.ring.submission().push(&sqe).map_err(|e| anyhow!("SQ full on splice out: {}", e))?;
        }
        self.ring.submit()?;
        Ok(())
    }
}

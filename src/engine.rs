use anyhow::{anyhow, Result};
use io_uring::{opcode, types, IoUring};
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, SockaddrIn};
use std::io::{Read, Write};
use std::net::ToSocketAddrs;
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;
use rustls::{ClientConfig, RootCertStore};
use std::path::Path;

use crate::http::{parse_http_response_header, ParsedUrl};
use crate::storage::DirectFileWriter;
use crate::pipe::KernelPipe;

const STATE_SEND_REQ: u8 = 1;
const STATE_READ_HEADER: u8 = 2;
const STATE_WRITE_INITIAL: u8 = 3;
const STATE_SPLICE_IN: u8 = 4;
const STATE_SPLICE_OUT: u8 = 5;

// Encode conn_id and state into u64 user_data
fn encode_user_data(conn_id: usize, state: u8) -> u64 {
    ((conn_id as u64) << 8) | (state as u64)
}

fn decode_user_data(user_data: u64) -> (usize, u8) {
    let state = (user_data & 0xFF) as u8;
    let conn_id = (user_data >> 8) as usize;
    (conn_id, state)
}

struct ConnectionState {
    id: usize,
    sock_fd: i32,
    pipe: KernelPipe,
    file_offset: u64,
    bytes_remaining: u64,
    
    // State machine buffers
    header_buf: Vec<u8>,
    header_bytes_read: usize,
    initial_payload_len: usize,
    
    // HTTP Request string to keep in memory for io_uring
    http_req: String,
    
    // Has this connection finished?
    done: bool,

    // Bounded buffer accounting
    pipe_bytes_available: usize,
    inflight_in_bytes: usize,
    inflight_out_bytes: usize,
    
    network_eof: bool,
    disk_eof: bool,
}

fn submit_producer(ring: &mut IoUring, state: &mut ConnectionState, buf_size: usize) -> Result<()> {
    if state.network_eof || state.inflight_in_bytes > 0 {
        return Ok(());
    }
    
    let pipe_space_free = state.pipe.capacity - state.pipe_bytes_available;
    if pipe_space_free > 0 {
        let mut max_read = std::cmp::min(pipe_space_free, state.bytes_remaining as usize);
        max_read = std::cmp::min(max_read, buf_size);
        
        if max_read > 0 {
            let sqe = opcode::Splice::new(
                types::Fd(state.sock_fd), -1,
                types::Fd(state.pipe.write_fd.as_raw_fd()), -1,
                max_read as u32,
            ).build().user_data(encode_user_data(state.id, STATE_SPLICE_IN));
            
            unsafe { ring.submission().push(&sqe).map_err(|e| anyhow!("SQ full: {}", e))?; }
            state.inflight_in_bytes = max_read;
        }
    }
    Ok(())
}

fn submit_consumer(ring: &mut IoUring, state: &mut ConnectionState, writer_fd: i32, buf_size: usize) -> Result<()> {
    if state.disk_eof || state.inflight_out_bytes > 0 {
        return Ok(());
    }
    
    if state.pipe_bytes_available > 0 {
        let mut max_write = state.pipe_bytes_available;
        max_write = std::cmp::min(max_write, buf_size);
        
        if max_write > 0 {
            let sqe = opcode::Splice::new(
                types::Fd(state.pipe.read_fd.as_raw_fd()), -1,
                types::Fd(writer_fd), state.file_offset as i64,
                max_write as u32,
            ).build().user_data(encode_user_data(state.id, STATE_SPLICE_OUT));
            
            unsafe { ring.submission().push(&sqe).map_err(|e| anyhow!("SQ full: {}", e))?; }
            state.inflight_out_bytes = max_write;
        }
    }
    Ok(())
}

pub struct DownloadEngine {
    ring: IoUring,
    buf_size: usize,
    connections: usize,
}

impl DownloadEngine {
    pub fn new(ring_entries: u16, buf_size: usize, connections: usize, _block_size_kb: usize) -> Result<Self> {
        let ring = IoUring::builder()
            .setup_cqsize(ring_entries as u32 * 4)
            .build(ring_entries as u32 * 2)?;

        Ok(Self {
            ring,
            buf_size,
            connections,
        })
    }
    
    fn setup_socket(url: &ParsedUrl) -> Result<i32> {
        let addr_str = format!("{}:{}", url.host, url.port);
        let socket_addr = addr_str
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow!("Failed to resolve IP for host: {}", url.host))?;

        let sock_fd = socket(AddressFamily::Inet, SockType::Stream, SockFlag::SOCK_CLOEXEC, None)?;
        let sockaddr_in = match socket_addr {
            std::net::SocketAddr::V4(v4) => SockaddrIn::from(v4),
            _ => return Err(anyhow!("IPv6 not supported in MVP")),
        };

        match connect(sock_fd.as_raw_fd(), &sockaddr_in) {
            Ok(_) | Err(nix::errno::Errno::EINPROGRESS) => {},
            Err(e) => return Err(anyhow!("Socket connection failed: {}", e)),
        }
        Ok(std::os::fd::IntoRawFd::into_raw_fd(sock_fd))
    }

    fn setup_ktls(sock_fd: i32, url: &ParsedUrl, rt: &tokio::runtime::Runtime) -> Result<Box<dyn std::any::Any>> {
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

        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(sock_fd) };
        std_stream.set_nonblocking(true)?;
        
        let ktls_stream = rt.block_on(async {
            let tokio_stream = tokio::net::TcpStream::from_std(std_stream).map_err(|e| anyhow!("from_std failed: {}", e))?;
            let corked_stream = ktls::CorkStream::new(tokio_stream);
            let connector = tokio_rustls::TlsConnector::from(config);
            let tls_stream = connector.connect(server_name, corked_stream).await.map_err(|e| anyhow!("TLS connect failed: {}", e))?;
            ktls::config_ktls_client(tls_stream).await.map_err(|e| anyhow!("kTLS setup failed: {:?}", e))
        })?;
        
        Ok(Box::new(ktls_stream))
    }

    fn get_file_info(url: &ParsedUrl) -> Result<u64> {
        let sock_fd = Self::setup_socket(url)?;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let _tls_guard = if url.scheme == "https" {
            Some(Self::setup_ktls(sock_fd, url, &rt)?)
        } else {
            None
        };
        
        let mut stream = unsafe { std::net::TcpStream::from_raw_fd(sock_fd) };
        stream.set_nonblocking(false).map_err(|e| anyhow!("set_nonblocking failed: {}", e))?;
        
        let head_req = format!(
            "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: ringdl/0.1.0\r\nAccept: */*\r\nRange: bytes=0-0\r\nConnection: close\r\n\r\n",
            url.path, url.host, url.port
        );
        stream.write_all(head_req.as_bytes()).map_err(|e| anyhow!("write_all failed: {}", e))?;
        
        let mut buf = vec![0; 8192];
        let n = stream.read(&mut buf).map_err(|e| anyhow!("read failed: {}", e))?;
        
        println!("DEBUG HEAD RESPONSE: {}", String::from_utf8_lossy(&buf[..n]));
        
        if let Some(header) = parse_http_response_header(&buf[..n])? {
            if let Some(len) = header.content_length {
                return Ok(len);
            }
        }
        
        Err(anyhow!("Failed to extract Content-Length from HEAD request"))
    }

    pub fn download(&mut self, url: &ParsedUrl, output_path: &Path) -> Result<()> {
        println!("📡 Pre-flight HEAD request to get file size...");
        let total_size = Self::get_file_info(url)?;
        println!("✨ Total file size: {} bytes", total_size);

        let num_connections = self.connections.max(1);
        let chunk_size = total_size / num_connections as u64;

        let w = DirectFileWriter::create(output_path, Some(total_size))?;
        let writer_fd = w.raw_fd();
        
        let mut states = Vec::with_capacity(num_connections);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let mut _tls_streams = Vec::with_capacity(num_connections);

        println!("🚀 Spawning {} concurrent connections...", num_connections);
        for id in 0..num_connections {
            let start = id as u64 * chunk_size;
            let end = if id == num_connections - 1 {
                total_size - 1
            } else {
                start + chunk_size - 1
            };
            
            let sock_fd = Self::setup_socket(url)?;
            if url.scheme == "https" {
                _tls_streams.push(Self::setup_ktls(sock_fd, url, &rt)?);
            }
            
            let http_req = format!(
                "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: ringdl/0.1.0\r\nAccept: */*\r\nRange: bytes={}-{}\r\nConnection: close\r\n\r\n",
                url.path, url.host, url.port, start, end
            );
            
            let pipe = KernelPipe::new()?;
            
            states.push(ConnectionState {
                id,
                sock_fd,
                pipe,
                file_offset: start,
                bytes_remaining: end - start + 1,
                header_buf: vec![0u8; 8192],
                header_bytes_read: 0,
                initial_payload_len: 0,
                http_req,
                done: false,
                pipe_bytes_available: 0,
                inflight_in_bytes: 0,
                inflight_out_bytes: 0,
                network_eof: false,
                disk_eof: false,
            });
        }
        
        println!("⚡ Engaging io_uring Multi-Connection Data Plane...");

        for state in states.iter() {
            let sqe = opcode::Write::new(
                types::Fd(state.sock_fd),
                state.http_req.as_ptr(),
                state.http_req.len() as u32,
            )
            .build()
            .user_data(encode_user_data(state.id, STATE_SEND_REQ));
            
            unsafe { self.ring.submission().push(&sqe).map_err(|e| anyhow!("SQ full: {}", e))?; }
        }
        self.ring.submit()?;

        let mut active_connections = num_connections;
        let mut total_downloaded_bytes: u64 = 0;

        while active_connections > 0 {
            self.ring.submit_and_wait(1)?;

            let cqes: Vec<(u64, i32, u32)> = self.ring
                .completion()
                .map(|cqe| (cqe.user_data(), cqe.result(), cqe.flags()))
                .collect();

            for (user_data, res, _) in cqes {
                let (conn_id, state_id) = decode_user_data(user_data);
                let mut state = &mut states[conn_id];
                
                if res < 0 {
                    let err = -res;
                    if err == libc::EAGAIN || err == libc::EWOULDBLOCK || err == libc::EINTR {
                        match state_id {
                            STATE_SEND_REQ => {
                                let sqe = opcode::Write::new(
                                    types::Fd(state.sock_fd),
                                    state.http_req.as_ptr(),
                                    state.http_req.len() as u32,
                                ).build().user_data(user_data);
                                unsafe { self.ring.submission().push(&sqe).map_err(|e| anyhow!("SQ full: {}", e))?; }
                            }
                            STATE_READ_HEADER => {
                                let sqe = opcode::Read::new(
                                    types::Fd(state.sock_fd),
                                    unsafe { state.header_buf.as_mut_ptr().add(state.header_bytes_read) },
                                    (state.header_buf.len() - state.header_bytes_read) as u32,
                                ).build().user_data(user_data);
                                unsafe { self.ring.submission().push(&sqe).map_err(|e| anyhow!("SQ full: {}", e))?; }
                            }
                            STATE_SPLICE_IN => {
                                state.inflight_in_bytes = 0;
                                submit_producer(&mut self.ring, state, self.buf_size)?;
                            }
                            STATE_SPLICE_OUT => {
                                state.inflight_out_bytes = 0;
                                submit_consumer(&mut self.ring, state, writer_fd, self.buf_size)?;
                            }
                            _ => {}
                        }
                        continue;
                    }
                    if err == libc::ECONNRESET && state.bytes_remaining == 0 {
                        if !state.done {
                            state.done = true;
                            active_connections -= 1;
                        }
                        continue;
                    } else {
                        return Err(anyhow!("io_uring error on conn {} state {}: errno {}", conn_id, state_id, err));
                    }
                }
                
                if res == 0 && state_id != STATE_SEND_REQ && state_id != STATE_WRITE_INITIAL {
                    if !state.done {
                        state.done = true;
                        active_connections -= 1;
                    }
                    continue;
                }

                match state_id {
                    STATE_SEND_REQ => {
                        let sqe = opcode::Read::new(
                            types::Fd(state.sock_fd),
                            unsafe { state.header_buf.as_mut_ptr().add(state.header_bytes_read) },
                            (state.header_buf.len() - state.header_bytes_read) as u32,
                        ).build().user_data(encode_user_data(conn_id, STATE_READ_HEADER));
                        unsafe { self.ring.submission().push(&sqe).map_err(|e| anyhow!("SQ full: {}", e))?; }
                    }
                    STATE_READ_HEADER => {
                        state.header_bytes_read += res as usize;
                        if let Some(header) = parse_http_response_header(&state.header_buf[..state.header_bytes_read])? {
                            let initial_payload = &state.header_buf[header.header_len..state.header_bytes_read];
                            state.initial_payload_len = initial_payload.len();
                            
                            if !initial_payload.is_empty() {
                                let sqe = opcode::Write::new(
                                    types::Fd(writer_fd),
                                    initial_payload.as_ptr(),
                                    initial_payload.len() as u32,
                                ).offset(state.file_offset as u64).build().user_data(encode_user_data(conn_id, STATE_WRITE_INITIAL));
                                unsafe { self.ring.submission().push(&sqe).map_err(|e| anyhow!("SQ full: {}", e))?; }
                            } else {
                                if state.bytes_remaining == 0 {
                                    state.network_eof = true;
                                    state.disk_eof = true;
                                    if !state.done {
                                        state.done = true;
                                        active_connections -= 1;
                                    }
                                } else {
                                    submit_producer(&mut self.ring, state, self.buf_size)?;
                                    submit_consumer(&mut self.ring, state, writer_fd, self.buf_size)?;
                                }
                            }
                        } else {
                            if state.header_bytes_read == state.header_buf.len() {
                                state.header_buf.resize(state.header_buf.len() * 2, 0);
                            }
                            let sqe = opcode::Read::new(
                                types::Fd(state.sock_fd),
                                unsafe { state.header_buf.as_mut_ptr().add(state.header_bytes_read) },
                                (state.header_buf.len() - state.header_bytes_read) as u32,
                            ).build().user_data(encode_user_data(conn_id, STATE_READ_HEADER));
                            unsafe { self.ring.submission().push(&sqe).map_err(|e| anyhow!("SQ full: {}", e))?; }
                        }
                    }
                    STATE_WRITE_INITIAL => {
                        let written = state.initial_payload_len as u64;
                        state.file_offset += written;
                        state.bytes_remaining = state.bytes_remaining.saturating_sub(written);
                        total_downloaded_bytes += written;
                        
                        if state.bytes_remaining == 0 {
                            state.network_eof = true;
                            state.disk_eof = true;
                            if !state.done {
                                state.done = true;
                                active_connections -= 1;
                            }
                        } else {
                            submit_producer(&mut self.ring, state, self.buf_size)?;
                            submit_consumer(&mut self.ring, state, writer_fd, self.buf_size)?;
                        }
                    }
                    STATE_SPLICE_IN => {
                        let bytes_read = res as usize;
                        state.pipe_bytes_available += bytes_read;
                        state.inflight_in_bytes = 0;
                        
                        state.bytes_remaining = state.bytes_remaining.saturating_sub(bytes_read as u64);
                        if state.bytes_remaining == 0 {
                            state.network_eof = true;
                        }
                        
                        submit_producer(&mut self.ring, state, self.buf_size)?;
                        submit_consumer(&mut self.ring, state, writer_fd, self.buf_size)?;
                    }
                    STATE_SPLICE_OUT => {
                        let bytes_written = res as usize;
                        state.pipe_bytes_available -= bytes_written;
                        state.inflight_out_bytes = 0;
                        
                        state.file_offset += bytes_written as u64;
                        total_downloaded_bytes += bytes_written as u64;
                        
                        let pct = (total_downloaded_bytes as f64 / total_size as f64) * 100.0;
                        print!("\rProgress: {} / {} bytes ({:.2}%)", total_downloaded_bytes, total_size, pct);
                        let _ = std::io::stdout().flush();
                        
                        if state.network_eof && state.pipe_bytes_available == 0 {
                            state.disk_eof = true;
                            if !state.done {
                                state.done = true;
                                active_connections -= 1;
                            }
                        } else {
                            submit_producer(&mut self.ring, state, self.buf_size)?;
                            submit_consumer(&mut self.ring, state, writer_fd, self.buf_size)?;
                        }
                    }
                    _ => {}
                }
            }
            // re-submit all queued SQEs
            self.ring.submit()?;
        }
        
        println!("\n🎉 Download complete via In-Kernel Zero-Copy Multi-Connection (splice)!");
        Ok(())
    }
}

# ringdl — In-Kernel Zero-Copy Downloader

`ringdl` is a high-performance, Linux-native TLS-offloaded transfer engine designed for infrastructure use cases. It achieves maximum theoretical download speeds with virtually zero **userspace CPU overhead** and page faults by leveraging `io_uring`, `splice(2)`, and Kernel TLS (`kTLS`).

Unlike `aria2c` or `curl`, which copy data through user-space buffers and `epoll` loops, `ringdl` orchestrates a **pure kernel-space data pipeline**. It multiplexes concurrent connections directly via Submission/Completion Queues (SQ/CQ) to slice decrypted data straight from the network socket into the disk controller.

## The Zero-Copy Pipeline

`ringdl` operates in two phases: a synchronous **Control Plane** and a highly-concurrent, zero-copy **Data Plane**.

### 1. Control Plane (Setup & TLS)
1. **Pre-flight & Handshake**: Resolves DNS, establishes TCP sockets, and negotiates TLS 1.2 via `rustls`.
2. **kTLS Offload**: Symmetric session keys are passed into the Linux kernel (`setsockopt(SOL_TLS)`). The kernel takes over all AES-GCM decryption seamlessly.
3. **Allocation**: Parses the HTTP headers to extract `Content-Range` and pre-allocates the exact disk space via `posix_fallocate()` to eliminate file fragmentation.

### 2. Data Plane (io_uring + splice)
Once setup is complete, the download multiplexes concurrent sockets via `io_uring` without relying on traditional `epoll` + read/write cycles. 

```text
[NIC RX Queue] ==(kTLS Decrypt)==> [Kernel Pipe] ==(splice)==> [Disk Page Cache]
```

For each HTTP chunk:
1. **`SPLICE_IN`**: `io_uring` executes `splice(2)`. The Linux kernel transparently decrypts the AES-GCM TLS records within the socket queue and transfers the `struct page *` memory references directly into a dedicated kernel pipe. *No physical payload bytes enter userspace.*
2. **`SPLICE_OUT`**: `io_uring` executes another `splice(2)`, injecting those exact decrypted page references from the pipe straight into the destination file's Page Cache.

## Benchmark: `ringdl` vs `aria2c`

### Methodology
- **Kernel**: Linux 7.1.3 (Debian ARM64 Cloud)
- **Architecture**: ARM64 virtualized
- **Disk**: `/dev/vda1` (Virtual Block Storage)
- **Network**: Local Docker bridge network (`172.18.0.x`), MTU 1500, simulating a 1Gbps environment.
- **Test**: 1GB payload over HTTPS, 10 concurrent connections. Target Nginx server rigidly rate-limited to 100 Mbps per connection.
- **aria2c command**: `aria2c -x 10 -s 10 -o aria2_bench.bin https://172.18.0.100:8443/test.bin`

| Metric | `aria2c` | `ringdl` | Breakdown |
| :--- | :--- | :--- | :--- |
| **Wall Clock Time** | 12.83s | **11.20s** | **12% Faster** |
| **Total CPU (User + Sys)** | **2.05s** | 3.24s | `ringdl` uses more *overall* CPU, but... |
| **User CPU Time** | 1.14s | 0.19s | ...`aria2c` spends its time in userspace. |
| **System CPU Time** | 0.91s | 3.05s | ...`ringdl` delegates TLS decryption (kTLS) to the kernel! |
| **Max RAM (RSS)** | 27.4 MB | **5.4 MB** | **80% Less RAM** |
| **Page Faults** | 9,361 | **569** | **94% Fewer Faults** |

## Usage

```bash
# Build
cargo build --release

# Download a file using 16 concurrent connections
target/release/ringdl -x 16 https://example.com/file.bin -o output.bin
```

### CLI Arguments
* `url`: Target HTTP/HTTPS URL.
* `-x, --connections <N>`: Concurrent HTTP Range connections (default: 16).
* `-o, --output <PATH>`: Output file path.
* `--buf-size <BYTES>`: Max splice chunk size per transaction (default: 1048576).
* `--ring-entries <N>`: Number of CQ/SQ completion ring entries (default: 128).

## Roadmap
* **Thorough Stability & Edge-Case Testing (TODO)**: Test edge cases (flaky networks, HTTP 302 redirects, massive terabyte files, servers missing `Content-Range` support).
* **IPv6 Support**: Extend socket resolution to handle `SocketAddr::V6`.
* **TLS 1.3 Support**: Bypass or intercept `NewSessionTicket` control records to support TLS 1.3 without crashing the pure byte-stream `splice` pipeline.

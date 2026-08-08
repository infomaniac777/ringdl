# ringdl — In-Kernel Zero-Copy Downloader

`ringdl` is a high-performance, Linux-native TLS-offloaded transfer engine designed for infrastructure use cases. It saturates shaped links while minimizing userspace CPU overhead and page faults by delegating TLS decryption and data movement to the kernel via `io_uring`, `splice(2)`, and Kernel TLS (`kTLS`).

Unlike `aria2c` or `curl`, which copy data through user-space buffers and `epoll` loops, `ringdl` orchestrates a **pure kernel-space data pipeline**. It multiplexes concurrent connections directly via Submission/Completion Queues (SQ/CQ) to slice decrypted data straight from the network socket into the disk controller.

## The Zero-Copy Pipeline

`ringdl` operates in two phases: a synchronous **Control Plane** and a highly-concurrent, zero-copy **Data Plane**.

### 1. Control Plane (Setup & TLS)
1. **Pre-flight & Handshake**: Resolves DNS, establishes TCP sockets, and negotiates TLS 1.2 via `rustls`.
2. **kTLS Offload**: Symmetric session keys are passed into the Linux kernel (`setsockopt(SOL_TLS)`). The kernel takes over all AES-GCM decryption seamlessly.
3. **Allocation**: Parses the HTTP headers to extract `Content-Range` and pre-allocates the exact disk space via `posix_fallocate()` to minimize allocation overhead during the download.

### 2. Data Plane (io_uring + splice)
Once setup is complete, the download multiplexes concurrent sockets via `io_uring` without relying on traditional `epoll` + read/write cycles. 

```text
[NIC RX Queue] ==(kTLS Decrypt)==> [Kernel Pipe] ==(splice)==> [Disk Page Cache]
```

For each HTTP chunk:
1. **`SPLICE_IN`**: `io_uring` executes `splice(2)`. Transfers pages into a dedicated kernel pipe—by page reference under hardware TLS offload, or via a kernel-internal copy of decrypted pages under software fallback (see kTLS Implementation Details). *No physical payload bytes enter userspace.*
2. **`SPLICE_OUT`**: `io_uring` executes another `splice(2)`, injecting those exact decrypted page references from the pipe straight into the destination file's Page Cache.

### kTLS Implementation Details
* **Record Framing**: TLS record boundaries are handled natively by kTLS. Partial records are buffered transparently in the socket layer until complete, ensuring `splice()` only operates on clean, fully decrypted plaintext stream boundaries.
* **Hardware vs Software Offload**: While `ringdl` completely eliminates user-space memory copying, true end-to-end zero-copy requires a NIC with Hardware TLS Offload. In standard or virtualized environments (like this benchmark), the kernel must fallback to software AES-GCM decryption. This forces the kernel to allocate new pages and perform an internal memory copy from the encrypted `sk_buff` to the decrypted pipe. This trade-off drastically reduces userspace CPU overhead, but shifts the cryptographic and allocation burden heavily onto the System CPU.

## Benchmark: `ringdl` vs `aria2c`

### Methodology
- **Kernel**: Linux 7.1.3 (Debian ARM64 Cloud)
- **Architecture**: 6-core ARM64 virtualized (ARMv8 Crypto Extensions: `aes`, `pmull` active)
- **Disk**: `/dev/vda1` (Virtual Block Storage, `ext4`)
- **Network**: Local Docker bridge network (`172.18.0.x`), MTU 1500, with injected WAN simulation (50ms latency, 0.1% packet loss via `tc netem`). Server (Nginx) and client are co-located on the same host.
- **Test**: 1GB payload over HTTPS, 10 concurrent connections. Target Nginx server rigidly rate-limited to 100 Mbps per connection.
- **Commands**:
```bash
aria2c -x 10 -s 10 -o aria2_bench.bin https://172.18.0.100:8443/test.bin
target/release/ringdl -x 10 https://172.18.0.100:8443/test.bin -o ringdl_bench.bin
```

| Metric | `aria2c` | `ringdl` | Breakdown |
| :--- | :--- | :--- | :--- |
| **Wall Clock Time** | 37.92s | **35.69s** | **Marginally Faster** (delta within run-to-run noise under randomized loss) |
| **Total CPU (User + Sys)** | **3.24s** | 4.53s | `ringdl` uses more *overall* CPU, because... |
| **User CPU Time** | 1.55s | 0.27s | ...`aria2c` spends its time in userspace. |
| **System CPU Time** | 1.69s | 4.26s | ...`ringdl` delegates software TLS decryption to the kernel. |
| **Max RAM (RSS)** | 26.3 MB | **5.4 MB** | **79% Less RAM** |
| **Page Faults** | 5,011 | **539** | **89% Fewer Faults** |

**Results:** single representative run; multi-run study with variance analysis pending (see Roadmap).

*Transparency Note on CPU usage: `ringdl` is designed to have virtually zero userspace CPU overhead, but this explicitly comes at the cost of higher System (Kernel) CPU time. Because this benchmark runs in a virtualized environment without hardware TLS offloading, the kernel is forced to perform AES-GCM software decryption and memory allocation for every packet before splicing it to disk. Furthermore, because the target Nginx server is co-located on the same VM, the CPU is performing AES-GCM encryption for the server and decryption for the client simultaneously.*

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
* **Thorough Performance Testing (TODO)**: Expand benchmarking and performance profiling against `aria2c` as a baseline across varied network conditions (latency, jitter, varying MTUs) to determine if this zero-copy architectural prototype warrants further development.

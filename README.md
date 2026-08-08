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

| Metric | `aria2c` (Median ± IQR) | `ringdl` (Median ± IQR) | Breakdown |
| :--- | :--- | :--- | :--- |
| **Wall Clock Time** | **31.11s ± 9.11s** | 67.31s ± 28.21s | **`aria2c` is 2.1x Faster** (See WAN Failure Analysis) |
| **Total CPU (User + Sys)** | **3.54s ± 0.78s** | 5.33s ± 0.95s | `ringdl` uses more *overall* CPU, because... |
| **User CPU Time** | 1.77s ± 0.23s | 0.29s ± 0.03s | ...`aria2c` spends its time in userspace. |
| **System CPU Time** | 1.77s ± 0.55s | 5.04s ± 0.92s | ...`ringdl` delegates software TLS decryption to the kernel. |
| **Max RAM (RSS)** | 26.8 MB ± 0.7 MB | **5.4 MB ± 0.01 MB** | **80% Less RAM** |
| **Page Faults** | 5,061 ± 522 | **548 ± 15** | **89% Fewer Faults** |

**Results:** Median and IQR of N=10 runs with page caches dropped (`drop_caches=3`) between every run.

### WAN Failure Analysis: The Impedance Mismatch
Under rigorous N=10 statistical testing with 50ms latency and 0.1% packet loss, `ringdl` collapses compared to `aria2c`. While `ringdl` is exceptionally fast on a local network, it suffers from severe **TCP Receive Window Starvation** over a WAN. 

Fundamentally, Network I/O (governed by TCP congestion control) and File I/O (governed by disk controllers and page cache writeback) operate in entirely different latency domains. 
1. `aria2c` allocates a massive 26MB userspace buffer. This buffer completely decouples the two domains. It acts as a shock absorber, aggressively draining the network socket even during disk I/O stalls, keeping the TCP window wide open for fast congestion control recovery.
2. `ringdl` restricts its memory buffer to a maximum 1MB kernel pipe per connection (`F_SETPIPE_SZ`). When the disk controller stalls even slightly, the 1MB pipe fills instantly. `SPLICE_IN` blocks, the socket queue fills up, and the Linux TCP stack immediately shrinks the advertised receive window to zero. 
3. This completely chokes the sender, ruining CUBIC's ability to recover from the 0.1% packet loss, resulting in extreme volatility (28s IQR) and double the download time.

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

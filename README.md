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

## Benchmark: `ringdl` vs `aria2c` (Decoupled Architecture)

### Methodology
- **Kernel**: Linux 7.1.3 (Debian ARM64 Cloud)
- **Architecture**: 6-core ARM64 virtualized (ARMv8 Crypto Extensions: `aes`, `pmull` active)
- **Disk**: `/dev/vda1` (Virtual Block Storage, `ext4`)
- **Network**: Local Docker bridge network (`172.18.0.x`), MTU 1500, with injected WAN simulation (50ms latency, 0.1% packet loss via `tc netem`). Server (Nginx) and client are co-located on the same host.
- **Test**: 10 GB payload over HTTPS, 4 concurrent connections. Target Nginx server rigidly rate-limited to 100 Mbps per connection.
- **Commands**:
```bash
aria2c -x 4 -s 4 -o aria2_bench.bin https://172.18.0.100:8443/test.bin
target/release/ringdl -x 4 https://172.18.0.100:8443/test.bin -o ringdl_bench.bin
```

| Metric | `aria2c` | `ringdl` (Decoupled) | Breakdown |
| :--- | :--- | :--- | :--- |
| **Wall Clock Time** | 4:10.14 | **3:58.28** | `ringdl` is now **faster**! (The kernel pipe shock absorber works) |
| **Total CPU (User + Sys)** | 32.65s | **27.00s** | `ringdl` uses **17% less total CPU**! |
| **User CPU Time** | 18.31s | **1.11s** | `ringdl` uses **94% less** userspace CPU. |
| **System CPU Time** | 14.34s | 25.89s | Expected kTLS software fallback overhead. |
| **Max RAM (RSS)** | 21.0 MB | **5.4 MB** | **74% Less RAM** |
| **Page Faults** | 35,151 | **1,089** | **97% Fewer Faults** |

### WAN Architecture Success: Decoupled io_uring Pipeline
Originally, a pure linear kernel pipeline was a dead end on WANs because it ping-ponged operations sequentially (`SPLICE_IN` then `SPLICE_OUT`), which coupled disk latency tightly to network latency, causing TCP Window Starvation.

To fix this, `ringdl` now uses a completely decoupled, asynchronous state machine! 
1. **Producer Loop**: Pushes network data into the kernel pipe (`SPLICE_IN`) as fast as possible.
2. **Consumer Loop**: Drains the pipe to disk (`SPLICE_OUT`) independently.
3. **16MB Bounded Buffer**: By leveraging a strict 16MB kernel pipe size limit (`F_SETPIPE_SZ`), the pipe acts as a massive shock absorber. It keeps the TCP Receive Window fully open even during multi-millisecond disk stalls, allowing CUBIC to properly recover from packet loss.


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
* **Deep-Pipeline Zero-Copy Redesign (DONE)**: Rewrote the `io_uring` state machine to fully decouple `SPLICE_IN` and `SPLICE_OUT`. A decoupled architecture maintains a continuous in-flight pipeline depth greater than the connection's BDP (Bandwidth-Delay Product), effectively using 16MB kernel pipes as a massive shock absorber against disk writeback jitter.
* **Hybrid Bounce Buffer Mode (TODO)**: Evaluate a hybrid design where a small userspace bounce buffer is implemented purely as a disk-side shock absorber for zero-copy transfers.
* **Hardware kTLS Validation (TODO)**: Re-run performance benchmarks on a bare-metal NIC equipped with true hardware TLS offloading, to measure the exact CPU savings when the kernel doesn't have to perform AES-GCM software fallback.
* **LAN/WAN Baseline Reporting (TODO)**: Restore the original LAN (0ms latency, 0% loss) benchmark table alongside the WAN results to fully contextualize the performance cliff.

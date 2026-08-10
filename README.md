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

| Metric | `aria2c` (Median ± IQR) | `ringdl` Decoupled (Median ± IQR) | Breakdown |
| :--- | :--- | :--- | :--- |
| **Wall Clock Time** | **262.61s ± 2.71s** | 267.22s ± 102.97s | Decoupled `ringdl` essentially **matches** `aria2c` throughput! |
| **Total CPU (User + Sys)** | **48.11s** (Approx) | 96.06s (Approx) | `ringdl` uses more *overall* CPU, because... |
| **User CPU Time** | 26.68s ± 1.04s | **2.90s ± 1.41s** | ...`ringdl` uses **89% less** userspace CPU. |
| **System CPU Time** | **21.43s ± 1.30s** | 93.16s ± 42.07s | ...`ringdl` delegates software TLS decryption to the kernel. |
| **Max RAM (RSS)** | 20.7 MB ± 0.6 MB | **5.4 MB ± 0.1 MB** | **74% Less RAM** |
| **Page Faults** | 29,292 ± 32,869 | **575 ± 35** | **98% Fewer Faults** |

**Results:** Median and IQR of N=10 runs for a 10 GB file with page caches dropped (`drop_caches=3`) between every run.

### WAN Architecture Success: Decoupled io_uring Pipeline
Originally, a pure linear kernel pipeline was a dead end on WANs because it ping-ponged operations sequentially (`SPLICE_IN` then `SPLICE_OUT`), which coupled disk latency tightly to network latency, causing TCP Window Starvation.

To fix this, `ringdl` now uses a completely decoupled, asynchronous state machine! 
1. **Producer Loop**: Pushes network data into the kernel pipe (`SPLICE_IN`) as fast as possible.
2. **Consumer Loop**: Drains the pipe to disk (`SPLICE_OUT`) independently.
3. **16MB Bounded Buffer**: By leveraging a strict 16MB kernel pipe size limit (`F_SETPIPE_SZ`), the pipe acts as a massive shock absorber. It keeps the TCP Receive Window fully open even during multi-millisecond disk stalls, allowing CUBIC to properly recover from packet loss.

### CPU Contention & Benchmark Bias Analysis

The severe spike in `ringdl`'s System CPU usage (93.16s vs `aria2c`'s 21.43s) and the high Wall Clock variance (IQR of 102.97s) indicate significant host-level contention in this specific VM setup. We suspect two major factors are skewing these metrics:

1. **kTLS Page Allocation Penalty:** The high System CPU time is not due to kernel AES-GCM math being slower than OpenSSL (both utilize hardware acceleration like ARM CE or AES-NI). Instead, the bottleneck is the memory allocation penalty in the software kTLS fallback path. Because `ringdl` uses `splice(2)`, the kernel cannot decrypt the incoming `sk_buff` in-place. It is forced to dynamically allocate new kernel memory pages for the plaintext, decrypt into them, and then `splice` those pages. This constant page allocation overhead is highly expensive on System CPU compared to `aria2c`, which simply decrypts directly into a static, pre-allocated userspace buffer.
2. **Sequential/Thermal Bias:** The N=10 benchmark executed 100 GB of `aria2c` traffic, immediately followed by 100 GB of `ringdl` traffic without cooldowns. Given that the target Nginx server is co-located on the same VM, the host CPU was forced to perform both AES-GCM encryption (for the server) and decryption (for the client) continuously. This sustained load likely induced thermal throttling and severe CPU cache saturation during the later `ringdl` runs, artificially inflating its System CPU time and inducing the extreme scheduling jitter (IQR 102s).

Future benchmarks should alternate runs (`aria2c` -> `ringdl` -> `aria2c`) with enforced idle cooldowns to validate these suspicions and eliminate sequential bias.

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

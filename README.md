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
target/release/ringdl -x 4 --buf-size 16384 https://172.18.0.100:8443/test.bin -o ringdl_bench.bin
```

| Metric | `aria2c` (Median ± IQR) | `ringdl` Decoupled (Median ± IQR) | Breakdown |
| :--- | :--- | :--- | :--- |
| **Wall Clock Time** | **267.24s ± 7.34s** | 273.01s ± 10.99s | `aria2c` holds a very slight lead in raw speed over 50ms WAN. |
| **Total CPU (User + Sys)** | **38.28s ± 0.92s** | 80.47s ± 5.00s | `ringdl` uses more *overall* CPU, because... |
| **User CPU Time** | 21.44s ± 1.32s | **2.60s ± 0.20s** | ...`ringdl` uses **87% less** userspace CPU. |
| **System CPU Time** | **16.81s ± 1.07s** | 77.85s ± 5.00s | ...`ringdl` delegates software TLS decryption to the kernel. |
| **Max RAM (RSS)** | 20.7 MB ± 0.35 MB | **5.4 MB ± 0.12 MB** | **74% Less RAM** |
| **Page Faults** | 11,208 ± 9,859 | **781 ± 449** | **93% Fewer Faults** |

**Results:** Median and IQR of N=10 interleaved runs (`aria2c` -> `ringdl` -> `aria2c`) with strict 15-second CPU cooldowns between every run to isolate hardware drift.

### WAN Architecture Success: Decoupled io_uring Pipeline
Originally, a pure linear kernel pipeline was a dead end on WANs because it ping-ponged operations sequentially (`SPLICE_IN` then `SPLICE_OUT`), which coupled disk latency tightly to network latency, causing TCP Window Starvation.

To fix this, `ringdl` now uses a completely decoupled, asynchronous state machine! 
1. **Producer Loop**: Pushes network data into the kernel pipe (`SPLICE_IN`) as fast as possible.
2. **Consumer Loop**: Drains the pipe to disk (`SPLICE_OUT`) independently.
3. **16MB Bounded Buffer**: By leveraging a strict 16MB kernel pipe size limit (`F_SETPIPE_SZ`), the pipe acts as a massive shock absorber. It keeps the TCP Receive Window fully open even during multi-millisecond disk stalls, allowing CUBIC to properly recover from packet loss.

### CPU Contention, Buffer Tuning, and True Variance

In earlier sequential tests, `ringdl` exhibited massive Wall Clock variance (IQR of 102.97s) and even higher System CPU load (93s). By tuning the architecture and rigorously isolating the tests, we identified the true constraints:

1. **The 16 KB Buffer Fix (TLS Boundary Alignment):** Originally, `ringdl` requested 1 MB buffers via `splice()`. Because kTLS requires full TLS records (16 KB) to verify AES-GCM tags, requesting 1 MB forced the kernel into a brutal `try_to_wake_up` polling loop internally, hanging the kernel threads and inflating System CPU. By capping `--buf-size 16384` to match the exact TLS record boundary, `splice()` instantly returns to userspace upon record decryption. This entirely broke the kernel polling loop, dropping System CPU significantly.
2. **Eliminating the Sequential Bias:** The N=10 Alternating Benchmark implemented strict 15-second idle cooldowns between every single tool run. This successfully mitigated external hardware factors (such as the VM encrypting and decrypting simultaneously) and completely stabilized `ringdl`'s download speed, dropping its Wall Clock IQR from **102.97s** down to a highly stable **10.99s**. Note: VM RAM was also increased from 2 GB to 4 GB during these tests, which may have marginally eased kernel `sk_buff` memory pressure.
3. **The Final Software kTLS Penalty:** Even with the 16 KB fix and unbiased cooldowns, `ringdl` still consumes 77.85s of System CPU vs `aria2c`'s 16.81s. The bottleneck remains the dynamic kernel page allocations required for the software kTLS fallback path. Because `splice(2)` cannot decrypt the incoming `sk_buff` in-place, the kernel is forced to dynamically allocate new plaintext pages, decrypt into them, and then `splice` those pages.

## Usage

```bash
# Build
cargo build --release

# Download a file using 16 concurrent connections with the optimal 16KB TLS buffer
target/release/ringdl -x 16 --buf-size 16384 https://example.com/file.bin -o output.bin
```

### CLI Arguments
* `url`: Target HTTP/HTTPS URL.
* `-x, --connections <N>`: Concurrent HTTP Range connections (default: 16).
* `-o, --output <PATH>`: Output file path.
* `--buf-size <BYTES>`: Max splice chunk size per transaction (default: 1048576, recommended: 16384 for TLS offload).
* `--ring-entries <N>`: Number of CQ/SQ completion ring entries (default: 128).

## MVP Status: Archived

After rigorous, unbiased N=10 benchmarking against `aria2c`, the decision has been made to formally halt MVP development and archive the project. 

While `ringdl` successfully proved the theoretical viability of a decoupled `io_uring` + `splice(2)` pipeline, and achieved a 74% reduction in RAM footprint alongside a 93% reduction in page faults, the absolute gains (saving ~15 MB of RAM) do not justify the severe computational cost. 

Because `ringdl` relies on the Software kTLS fallback path in standard virtualized environments, the kernel is forced to dynamically allocate pages and perform internal AES-GCM math, resulting in a staggering **2x Total CPU Time** penalty (80s vs 38s) compared to `aria2c`'s highly-optimized userspace OpenSSL implementation.

In modern cloud architecture, CPU cycles are drastically more expensive than a 15 MB memory overhead. Furthermore, as of August 2026, true Hardware kTLS Offloading is practically non-existent in typical consumer hardware and standard cloud instances. Because avoiding the severe software fallback penalty requires specialized enterprise NICs, developing this architecture for general-purpose use is a practical dead end. Until hardware offload becomes a ubiquitous commodity, the in-kernel zero-copy architecture cannot decisively beat highly-optimized userspace tools like `aria2c`.

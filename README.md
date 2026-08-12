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
### 10 GB File Benchmark (Median of N=10)

| Metric | `aria2c` | `ringdl` (16 KB Net / 1 MB Disk) |
| :--- | :--- | :--- |
| **Wall Clock Time** | **267.40s** | 272.76s |
| **Total CPU (User + Sys)** | **37.11s** | 47.29s |
| **User CPU Time** | 21.40s | **1.04s** |
| **System CPU Time** | **15.89s** | 46.20s |
| **Max RAM (RSS)** | 20.9 MB | **5.4 MB** |
| **Page Faults** | 9,939 | **579** |

### 1 GB File Benchmark (HTTPS / TLS, Random Entropy)

| Metric | `aria2c` | `ringdl` (16 KB Net / 1 MB Disk) |
| :--- | :--- | :--- |
| **Wall Clock Time** | **26.18s** | 27.77s |
| **Total CPU (User + Sys)** | **3.57s** | 5.29s |
| **User CPU Time** | 2.04s | **0.18s** |
| **System CPU Time** | **1.56s** | 5.08s |
| **Max RAM (RSS)** | 20.6 MB | **5.4 MB** |
| **Page Faults** | 4,540 | **503** |

### 1 GB File Benchmark (HTTP / Non-TLS, Random Entropy)

| Metric | `aria2c` | `ringdl` (16 KB Net / 1 MB Disk) |
| :--- | :--- | :--- |
| **Wall Clock Time** | **24.52s** | 25.21s |
| **Total CPU (User + Sys)** | **2.44s** | 3.47s |
| **User CPU Time** | 0.54s | **0.07s** |
| **System CPU Time** | **1.90s** | 3.37s |
| **Max RAM (RSS)** | 20.5 MB | **3.1 MB** |
| **Page Faults** | 3,031 | **178** |

**Results:** Both benchmarks used N=10 interleaved runs (`aria2c` -> `ringdl` -> `aria2c`) with strict 15-second CPU cooldowns between every run to isolate hardware drift. The `ringdl` parameters used were `--buf-size 16384` for the network `splice_in`, and a hardcoded 1 MB threshold for the disk `splice_out`.

### WAN Architecture Success: Decoupled io_uring Pipeline
Originally, a pure linear kernel pipeline was a dead end on WANs because it ping-ponged operations sequentially (`SPLICE_IN` then `SPLICE_OUT`), which coupled disk latency tightly to network latency, causing TCP Window Starvation.

To fix this, `ringdl` now uses a completely decoupled, asynchronous state machine! 
1. **Producer Loop**: Pushes network data into the kernel pipe (`SPLICE_IN`) as fast as possible.
2. **Consumer Loop**: Drains the pipe to disk (`SPLICE_OUT`) independently.
3. **16MB Bounded Buffer**: By leveraging a strict 16MB kernel pipe size limit (`F_SETPIPE_SZ`), the pipe acts as a massive shock absorber. It keeps the TCP Receive Window fully open even during multi-millisecond disk stalls, allowing CUBIC to properly recover from packet loss.

### CPU Contention, Buffer Tuning, and True Variance

In earlier tests, `ringdl` exhibited massive Wall Clock variance and even higher System CPU load (77.85s). By tuning the architecture and rigorously isolating the tests, we identified the following optimizations and bottlenecks:

1. **The 16 KB Buffer Fix (TLS Boundary Alignment):** Originally, `ringdl` requested 1 MB network buffers via `splice()`. Because kTLS requires full TLS records (16 KB) to verify AES-GCM tags, asking for 64 KB or 1 MB stalled the kernel threads and caused the Wall Clock time to skyrocket. By perfectly aligning `--buf-size 16384` to match the exact TLS record boundary, the pipeline flows with zero stall latency.
2. **The 1 MB Disk Write Threshold:** Originally, `ringdl` flushed to the disk page cache as soon as the 16 KB network payload arrived. This flooded the kernel with millions of micro-writes. By forcing the internal kernel pipe to buffer 1 MB before issuing a `splice_out` to the file, the System CPU overhead plummeted from 77.85s down to 46.20s.
3. **The Degradation Hypothesis:** While a single isolated run of `ringdl` achieved a System Time of just 18.67s (nearly matching `aria2c`), the N=10 median skyrocketed to 46.20s over sustained load. We hypothesize that this is caused by severe kernel slab allocator fragmentation. Software kTLS must dynamically allocate fresh plaintext pages for every decrypted 16 KB chunk. `ringdl` then moves these pages into the kernel pipe, and finally into the Page Cache. Over 100 GB of continuous transfer, we suspect the cost of allocating, tracking, and freeing millions of small page references fragments the memory subsystem, causing the kernel to burn massive CPU cycles just managing the page pipeline.

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

While `ringdl` successfully proved the theoretical viability of a decoupled `io_uring` + `splice(2)` pipeline, and achieved a 74% reduction in RAM footprint alongside a 94% reduction in page faults, the absolute gains (saving ~15 MB of RAM) do not justify the severe computational cost. 

Because `ringdl` relies on the Software kTLS fallback path in standard virtualized environments, the kernel is forced to dynamically allocate pages and perform internal AES-GCM math. Combined with the hypothesized page management overhead and fragmentation under sustained load, this results in a **27% Total CPU Time penalty** (47.29s vs 37.11s) compared to `aria2c`'s highly-optimized userspace copying.

In modern cloud architecture, CPU cycles are drastically more expensive than a 15 MB memory overhead. Until Hardware kTLS Offload becomes a ubiquitous commodity on consumer and standard cloud NICs, allowing true in-place decryption without page shuffling, the in-kernel zero-copy architecture cannot decisively beat highly-optimized userspace tools like `aria2c`.

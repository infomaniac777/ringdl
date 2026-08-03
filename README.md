# Project ringdl — MVP Architecture & Design Specification

## 1. Executive Summary & Goal
`ringdl` is a high-performance HTTP file downloader built for modern Linux environments. The core goal is to maximize throughput while minimizing CPU overhead and context switches by eliminating redundant user-kernel memory copies and traditional POSIX `epoll` / `read()` syscall loops.

Instead of operating as a traditional data copier, `ringdl` acts as an **I/O orchestrator**, coordinating hardware DMA from the Network Interface Card (NIC) into user-space memory and out to storage controllers via `io_uring` and Direct I/O (`O_DIRECT`).

---

## 2. Technical Architecture

### 2.1 Overview & Data Pipeline Flow
```
+-----------------------------------------------------------------------------------+
|                                  ringdl Data Path                                 |
+-----------------------------------------------------------------------------------+
|                                                                                   |
|  +--------------------+    TCP Stream    +-------------------------------------+  |
|  | Plain HTTP/1.1     | ---------------> | io_uring Receive Engine             |  |
|  | Raw Socket Engine  |                  | Tier 1: IORING_OP_RECV_ZC (ZCRX)    |  |
|  +--------------------+                  | Tier 2: io_uring_buf_ring Multishot |  |
|                                          +-------------------------------------+  |
|                                                             |                     |
|                                                             v                     |
|                                          +-------------------------------------+  |
|                                          | Aligned Staging & Header Parser     |  |
|                                          | - Parse HTTP response headers       |  |
|                                          | - Align TCP stream to block sectors |  |
|                                          +-------------------------------------+  |
|                                                             |                     |
|                                                             v                     |
|  +--------------------+    O_DIRECT      +-------------------------------------+  |
|  | Destination File   | <--------------- | Storage Write Queue                 |  |
|  | (fallocate pre-alloc)|                | IORING_OP_WRITE_FIXED / WRITEV      |  |
|  +--------------------+                  +-------------------------------------+  |
+-----------------------------------------------------------------------------------+
```

---

## 3. Core Technical Nuances & Engineering Design

### 3.1 Storage Sector Alignment (`O_DIRECT`) vs. TCP Streaming
* **Constraint**: Opening destination files with `O_DIRECT` bypasses the Linux page cache, requiring:
  1. Memory buffer address aligned to storage sector boundary (512 or 4096 bytes).
  2. File write offset aligned to sector boundary.
  3. Transfer size aligned to sector boundary.
* **Stream Aggregation Strategy**:
  * TCP packets arrive in arbitrary sizes (MSS ~1460 bytes, HTTP header offsets, etc.).
  * `ringdl` implements an **Aligned Staging Aggregator** in user space.
  * Incoming TCP payloads are accumulated into aligned memory block boundaries (e.g., 64 KiB or 128 KiB).
  * Full blocks are written to disk via `O_DIRECT`. Any unaligned trailing bytes at the end of a download are written via aligned buffer padding or buffered fallback.

### 3.2 Tiered Receive Engine
1. **Tier 1: Hardware Zero-Copy Receive (`IORING_OP_RECV_ZC` / `ZCRX`)**:
   * Uses Linux kernel 6.12+ `IORING_REGISTER_ZCRX` with memory provider user-space buffers.
   * Requires NIC driver support for header/data split (`ethtool -K <iface> tcp-data-split on`).
2. **Tier 2: Software Zero-Syscall Fallback (`io_uring_buf_ring`)**:
   * If ZCRX registration is unsupported by the NIC or kernel config, `ringdl` gracefully falls back to `io_uring_buf_ring` (`IORING_OP_RECV` multishot).
   * Eliminates per-packet `read()` syscall overhead while ensuring high performance across all Linux hardware.

### 3.3 File Pre-Allocation (`IORING_OP_FALLOCATE`)
* Upon receiving HTTP response headers (`HTTP/1.1 200 OK` or `206 Partial Content`) and extracting `Content-Length`, `ringdl` immediately submits an `IORING_OP_FALLOCATE` SQE.
* Pre-allocates contiguous disk space on storage to prevent filesystem fragmentation during high-speed writes.

---

## 4. Implementation Language & Technology Choice

* **Language**: **Rust**
* **Rationale**:
  * Strict compile-time lifetime and borrowing enforcement prevents memory corruption or data races when managing raw memory buffers registered across kernel rings and storage queues.
  * Zero-cost abstractions with standard FFI capabilities to bind directly to Linux kernel `io_uring` header structures when crate ecosystems lag behind bleeding-edge kernel releases.

---

## 5. MVP Scope & Phased Implementation Plan

### Phase 1: HTTP/1.1 Engine & Buffer Ring Storage Write (Tier 2 Baseline)
- [ ] Initialize Rust project structure.
- [ ] Implement socket connection & HTTP/1.1 request formatting.
- [ ] Implement `io_uring` ring setup and `io_uring_buf_ring` multishot receive loop.
- [ ] Build HTTP header parser (`httparse`) to extract `Content-Length` and isolate payload boundary (`\r\n\r\n`).
- [ ] Implement `O_DIRECT` file creation, `IORING_OP_FALLOCATE` pre-allocation, and block-aligned `IORING_OP_WRITE_FIXED` pipeline.

### Phase 2: Experimental Hardware Zero-Copy Receive (Tier 1 Integration)
- [ ] Add raw FFI bindings for `IORING_REGISTER_ZCRX` and `IORING_OP_RECV_ZC`.
- [ ] Implement runtime probing for NIC ZCRX capability with automatic fallback to Tier 2.

### Phase 3: Benchmarking & Profiling
- [ ] Build performance harness comparing throughput, CPU utilization, and context switches against `curl` and `aria2c`.

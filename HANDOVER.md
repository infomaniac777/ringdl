# `ringdl` — High-Performance Zero-Copy `io_uring` Downloader

This document serves as a complete technical handover, architecture reference, and benchmark summary for the **ringdl** project.

---

## 1. Architectural Overview

`ringdl` is a single-threaded, zero-copy HTTP downloader written in Rust that leverages modern Linux 5.19+ / 6.x `io_uring` kernel features:

```
+-----------------------------------------------------------------------------------+
|                              Linux Kernel (io_uring)                              |
|                                                                                   |
|  +-----------------------+                         +---------------------------+  |
|  |  io_uring_buf_ring    | --(1. Packet Arrives)-> |  IORING_OP_RECV_MULTI     |  |
|  |  (Provided Buf Ring)  |                         |  (Multishot Receive CQE)  |  |
|  +-----------------------+                         +---------------------------+  |
|              ^                                                   |                |
|              |                                        (2. Zero-Copy Pointer       |
|              |                                            Handoff in User Space)  |
|              |                                                   v                |
|     (4. Recycle Buffer                               +---------------------------+  |
|        to Buf Ring)                                  |  IORING_OP_WRITE          |  |
|              |                                       |  (Storage Write SQE)      |  |
|              |                                       +---------------------------+  |
|              |                                                   |                |
|              +<-------- (3. Storage DMA Completed) <-------------+                |
+-----------------------------------------------------------------------------------+
```

### Key Architectural Pillars
1. **Provided Buffer Rings (`io_uring_buf_ring`):**
   * Registered with kernel background ID `BUF_BGID = 1`.
   * Default configuration: **128 entries** × **128 KiB buffer size** (16 MiB total buffer ring memory).
2. **Multishot Network Receive (`IORING_OP_RECV_MULTI`):**
   * A single multishot receive SQE is armed on the TCP socket. The Linux kernel automatically dequeues buffers from `io_uring_buf_ring` when packets arrive and emits CQEs containing the Buffer ID (`bid`).
3. **Zero-Copy Pointer Handoff to Storage (`IORING_OP_WRITE`):**
   * When a receive CQE arrives with payload data, the application **never copies or inspects the memory in user space**.
   * The raw kernel buffer pointer (`buf_ptr`) is passed directly to an `IORING_OP_WRITE` SQE targeting the file descriptor at `writer.current_offset()`.
4. **SQE Submission Batching:**
   * Instead of invoking `ring.submit()` after each individual disk write SQE, write SQEs are queued during the CQE batch processing loop and submitted **once per completion batch** via a single `io_uring_enter` syscall.

---

## 2. Why Page Cache Mode (Not `O_DIRECT`)

We evaluated both **`O_DIRECT`** and **Page Cache (`O_RDWR | O_CREAT | O_TRUNC`)** modes. We selected **Page Cache mode** as the primary architecture for the following technical reasons:

### The `O_DIRECT` Alignment Constraint
In Linux, `O_DIRECT` strictly requires:
1. Memory addresses (`buf_ptr`) aligned to 4096 bytes.
2. Write lengths (`bytes_read`) aligned to 4096 bytes.
3. File offsets aligned to 4096 bytes.

### Why TCP Streams Violate `O_DIRECT`
* **HTTP Header Offsets:** When parsing the HTTP/1.1 response header, the payload starts at an arbitrary byte offset (e.g., `buf_ptr + 153`), making the memory address and first chunk length unaligned.
* **Arbitrary TCP Frame Sizes:** TCP stream packets arriving from the network via `IORING_OP_RECV_MULTI` come in arbitrary MTU frame sizes (1460, 1500, 2920, 64240 bytes) that are almost never multiples of 4096 bytes.
* **Consequence:** Using `O_DIRECT` on arbitrary TCP streams causes `io_uring` to reject writes with `-libc::EINVAL` (errno 22) unless an intermediate user-space staging/accumulation buffer is used, which re-introduces CPU memory copying (`memcpy`).

### Page Cache Advantages
* In Page Cache mode, raw network buffer pointers are handed directly to disk write SQEs with **zero memory copying in user space** (`0.07s` User CPU time).
* The OS page cache absorbs arbitrary-sized writes at arbitrary offsets and handles block alignment and asynchronous background flushing (`kswapd` / `flush`) automatically.

---

## 3. 5 GB Ethernet Simulation Benchmark

Benchmark results for a 5.00 GiB (`5,242,880,000 bytes`) download over a 1500-MTU virtual Ethernet (`veth`) network interface (`10.200.1.2:8085`):

| Metric | `aria2c` (5 GB) | `ringdl` (4 KiB + unbatched) | **`ringdl` (128 KiB + Batched SQE)** |
| :--- | :--- | :--- | :--- |
| **User CPU Time (s)** | 0.61s | ~0.10s | **0.07s** (8.7x lower than aria2c!) |
| **Kernel/System CPU Time (s)** | 2.16s | 8.19s | **3.52s** (**2.3x faster** than unbatched) |
| **Total CPU Time (User+Sys)** | 2.77s | 8.29s | **3.59s** |
| **CPU Percentage** | 27% | ~60% | **37%** |
| **Voluntary Context Switches** | 31,278 | 98,321 | **89,832** |
| **Max RSS Memory (KB)** | 17,552 KB | 18,520 KB | **18,532 KB** |
| **SHA-256 Verification** | `264348bad...` | `264348bad...` | **`264348bad...` (100% Match)** |

### Understanding Kernel CPU Time & Voluntary Context Switches
* **Why `ringdl` has higher context switches (`89,832` vs `31,278`):**
  * When `io_uring` executes buffered file writes (`IORING_OP_WRITE` without `O_DIRECT`), the Linux kernel frequently offloads the write to an internal kernel worker thread pool (`io_wq`) if inode mutexes or page allocation locks are contested.
  * Waking `io_wq` kernel threads for ~40,000 buffered writes (5 GiB / 128 KiB) accounts for the voluntary context switches and kernel `sys` time (`3.52s`).
* **Why `aria2c` has lower kernel time (`2.16s`):**
  * `aria2c` uses synchronous POSIX `write()` syscalls from user-space threads directly into the page cache, avoiding async `io_wq` queue transitions.
* **Why `ringdl` dominates User CPU Time (`0.07s` vs `0.61s`):**
  * Because `ringdl` passes raw kernel memory pointers directly between receive and write SQEs without user-space copying or buffer manipulation, application-layer CPU time is virtually zero.

---

## 4. Code & Configuration Reference

* **CLI Defaults (`src/main.rs`):**
  * `--ring-entries`: `128`
  * `--buf-size`: `131072` (128 KiB)
  * `--block-size-kb`: `64`
* **Core Engine (`src/engine.rs`):**
  * Implements `DownloadEngine`, multishot receive ring setup, SQE batching, and zero-copy write dispatch.
* **Storage Layer (`src/storage.rs`):**
  * Implements `DirectFileWriter` with `posix_fallocate` disk space pre-allocation and `OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_TRUNC`.

---

## 5. Roadmap & Next Steps

When resuming development on a new environment, consider the following next optimizations:
1. **Test Larger Buffer Chunk Sizes (256 KiB / 512 KiB):**
   * Increasing `--buf-size` to `262144` (256 KiB) or `524288` (512 KiB) will reduce the total number of storage write operations by 2x–4x, further reducing `io_wq` thread scheduling and voluntary context switches.
2. **Multi-Connection HTTP/1.1 Downloading:**
   * Support HTTP Range requests to open multiple TCP sockets concurrently within the single `IoUring` instance.
3. **IPv6 Support:**
   * Extend TCP socket connection resolution in `engine.rs` to handle `SocketAddr::V6`.

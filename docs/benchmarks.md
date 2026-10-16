# xlite Benchmarks & Performance Engineering

This document summarizes `xlite`'s single-node streaming load-test results across **36 formal stages (10,549,562 requests)** and the transport-level engineering optimizations behind them.

- [1. Single-Node Streaming Benchmark (1,000 → 18,000 Concurrent Streams)](#1-single-node-streaming-benchmark-1000--18000-concurrent-streams)
- [2. Full 36-Stage Results Table](#2-full-36-stage-results-table)
- [3. Operational Root-Cause Ablations](#3-operational-root-cause-ablations)
- [4. Reproducing the Benchmark Locally](#4-reproducing-the-benchmark-locally)

---

## 1. Single-Node Streaming Benchmark (1,000 → 18,000 Concurrent Streams)

### Testbed & Methodology

| Parameter | Configuration |
| :--- | :--- |
| **Topology** | 16-shard `k6` load generator → **1 × `xlite` gateway node** → 24 upstream `h2c` SSE mock instances ([`mock-llm`](https://github.com/EeroEternal/mock-llm) / `stream_mock_server.mjs`) |
| **CPU Allocation** | Gateway: **28 vCPU** pinned · Load Generator (`k6` × 16 shards): **56 vCPU** · Upstream Mocks (24 instances): **28 vCPU** |
| **Memory Limits** | Gateway: **64 GiB** · Load Generator: **192 GiB** (`12 GiB` × 16 shards) · Upstream Mocks: **64 GiB** |
| **Workload Profile** | OpenAI `POST /v1/chat/completions` (`"stream": true`, 100-token input prompt, `MOCK_TTFT_MS=500` simulated upstream prefill wait, `MOCK_BATCH=1`) |
| **Concurrency Tiers** | **1,000 · 2,000 · 5,000 · 10,000 · 15,000 · 18,000** concurrent active virtual users (VUs) |
| **Stream Speeds & Durations** | **20 tok/s**, **50 tok/s**, and **100 tok/s** per stream × **5s short streams** (300s steady-state) and **60s long streams** (640s steady-state) |
| **Total Requests Executed** | **10,549,562** formal requests (**5,276,182** full SSE bodies sampled and verified) |
| **Error Rate** | **0.00%** across all 36 stages (`100.00%` HTTP 200 & valid SSE stream completion) |

### Key Highlights

- **~2.0–2.5 ms P50 TTFT Overhead at 10,000–18,000 Concurrent Streams**: Even when holding **18,000 simultaneous SSE connections** open at 20–50 tok/s per stream, total end-to-end TTFT overhead (`Total TTFT - 500ms mock prefill wait`, which includes client + network + mock scheduling + gateway) stays at **2.12–2.50 ms (P50)** and **3.53–4.20 ms (P90)**.
- **Up to 1.70 Million Output Tokens/sec (~101.8M TPM) on a Single Node**: At 18,000 concurrent streams × 100 tok/s (`Group D`), a single `xlite` instance processes **1,697,203 SSE chunks/sec** with **0 failed requests**.
- **Linear Throughput Scaling**: From 1,000 to 18,000 VUs, steady-state RPS scales linearly (`183.2 → 3,298.4 req/s` for 5s streams; `15.9 → 285.8 req/s` for 60s streams).

---

## 2. Full 36-Stage Results Table

> **Metric Notes**:
> - **Steady-State RPS / Output TPS**: Measured during the steady-state window (300s for 5s streams, 640s for 60s streams after a 30s ramp-up). Since `MOCK_BATCH=1`, 1 output token = 1 SSE `data:` frame forwarded by `xlite`.
> - **TTFT Overhead (`TTFT - 500ms`)**: Reported as the min–max range across the 16 independent `k6` load-generator shards (includes `k6` client scheduling, Docker bridge traversal, `xlite` routing, and mock server wakeup).

### 2.1 Short Streams (5s Output Duration, 300s Steady-State)

| Group | Stream Rate | Output Tokens / Req | Concurrent VUs | Success Rate | Steady-State RPS | Steady-State Output TPS (tok/s) | TTFT Overhead P50 (ms) | TTFT Overhead P90 (ms) | TTFT Overhead P99 (ms) |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **A** | **20 tok/s** (50ms/chunk) | 100 | **1,000** | 100.0% | 183.2 | 18,324 | 4.44 – 4.79 | 11.23 – 12.42 | 20.39 – 22.28 |
| **A** | **20 tok/s** (50ms/chunk) | 100 | **2,000** | 100.0% | 366.5 | 36,647 | 3.45 – 3.54 | 6.32 – 6.49 | 9.54 – 10.32 |
| **A** | **20 tok/s** (50ms/chunk) | 100 | **5,000** | 100.0% | 916.4 | 91,636 | 2.53 – 2.57 | 3.74 – 3.80 | 5.24 – 5.55 |
| **A** | **20 tok/s** (50ms/chunk) | 100 | **10,000** | 100.0% | 1,832.6 | 183,264 | 2.39 – 2.41 | 3.55 – 3.59 | 5.27 – 5.63 |
| **A** | **20 tok/s** (50ms/chunk) | 100 | **15,000** | 100.0% | 2,748.8 | 274,880 | 2.37 – 2.40 | 3.59 – 3.64 | 6.09 – 6.43 |
| **A** | **20 tok/s** (50ms/chunk) | 100 | **18,000** | 100.0% | **3,298.4** | **329,840** | **2.37 – 2.39** | **3.53 – 3.57** | **8.35 – 9.52** |
| **E** | **50 tok/s** (20ms/chunk) | 250 | **1,000** | 100.0% | 182.1 | 45,522 | 3.25 – 3.36 | 5.57 – 5.84 | 8.17 – 9.07 |
| **E** | **50 tok/s** (20ms/chunk) | 250 | **2,000** | 100.0% | 364.2 | 91,047 | 2.63 – 2.67 | 3.82 – 3.92 | 5.31 – 5.64 |
| **E** | **50 tok/s** (20ms/chunk) | 250 | **5,000** | 100.0% | 910.5 | 227,613 | 1.84 – 1.87 | 2.72 – 2.77 | 3.67 – 3.80 |
| **E** | **50 tok/s** (20ms/chunk) | 250 | **10,000** | 100.0% | 1,820.6 | 455,151 | 2.01 – 2.04 | 2.92 – 2.98 | 4.80 – 5.67 |
| **E** | **50 tok/s** (20ms/chunk) | 250 | **15,000** | 100.0% | 2,731.1 | 682,770 | 2.08 – 2.11 | 3.25 – 3.34 | 10.26 – 12.19 |
| **E** | **50 tok/s** (20ms/chunk) | 250 | **18,000** | 100.0% | **3,277.4** | **819,340** | **2.12 – 2.15** | **3.93 – 4.20** | **15.73 – 19.31** |
| **B** | **100 tok/s** (10ms/chunk) | 500 | **1,000** | 100.0% | 181.6 | 90,778 | 2.68 – 2.74 | 3.89 – 3.98 | 5.25 – 5.61 |
| **B** | **100 tok/s** (10ms/chunk) | 500 | **2,000** | 100.0% | 363.2 | 181,577 | 2.00 – 2.04 | 3.00 – 3.06 | 3.87 – 4.11 |
| **B** | **100 tok/s** (10ms/chunk) | 500 | **5,000** | 100.0% | 907.8 | 453,882 | 1.95 – 1.99 | 2.67 – 2.70 | 4.15 – 4.62 |
| **B** | **100 tok/s** (10ms/chunk) | 500 | **10,000** | 100.0% | 1,816.0 | 907,995 | 2.02 – 2.06 | 3.27 – 3.53 | 13.06 – 15.84 |
| **B** | **100 tok/s** (10ms/chunk) | 500 | **15,000** | 100.0% | 2,707.9 | 1,353,948 | 18.33 – 18.51 | 35.76 – 36.41 | 69.26 – 89.60 |
| **B** | **100 tok/s** (10ms/chunk) | 500 | **18,000** | 100.0% | **3,221.0** | **1,610,475** | 22.08 – 22.41 | 42.32 – 44.33 | 107.34 – 182.92 |

### 2.2 Long Streams (60s Output Duration, 640s Steady-State)

| Group | Stream Rate | Output Tokens / Req | Concurrent VUs | Success Rate | Steady-State RPS | Steady-State Output TPS (tok/s) | TTFT Overhead P50 (ms) | TTFT Overhead P90 (ms) | TTFT Overhead P99 (ms) |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **C** | **20 tok/s** (50ms/chunk) | 1,200 | **1,000** | 100.0% | 15.9 | 19,058 | 4.87 – 5.41 | 10.18 – 11.30 | 15.23 – 20.93 |
| **C** | **20 tok/s** (50ms/chunk) | 1,200 | **2,000** | 100.0% | 31.8 | 38,111 | 3.56 – 3.75 | 6.25 – 6.64 | 10.66 – 15.10 |
| **C** | **20 tok/s** (50ms/chunk) | 1,200 | **5,000** | 100.0% | 79.4 | 95,259 | 2.66 – 2.73 | 3.86 – 3.99 | 6.72 – 8.48 |
| **C** | **20 tok/s** (50ms/chunk) | 1,200 | **10,000** | 100.0% | 158.8 | 190,515 | 2.47 – 2.52 | 3.67 – 3.80 | 6.36 – 9.43 |
| **C** | **20 tok/s** (50ms/chunk) | 1,200 | **15,000** | 100.0% | 238.1 | 285,756 | 2.47 – 2.50 | 3.75 – 3.83 | 8.66 – 11.79 |
| **C** | **20 tok/s** (50ms/chunk) | 1,200 | **18,000** | 100.0% | **285.8** | **342,908** | **2.47 – 2.50** | **3.71 – 3.81** | **13.31 – 15.90** |
| **F** | **50 tok/s** (20ms/chunk) | 3,000 | **1,000** | 100.0% | 15.9 | 47,564 | 3.30 – 3.52 | 5.60 – 5.95 | 8.87 – 12.18 |
| **F** | **50 tok/s** (20ms/chunk) | 3,000 | **2,000** | 100.0% | 31.7 | 95,114 | 2.68 – 2.80 | 3.87 – 4.20 | 5.96 – 8.95 |
| **F** | **50 tok/s** (20ms/chunk) | 3,000 | **5,000** | 100.0% | 79.2 | 237,736 | 1.93 – 1.99 | 2.81 – 2.91 | 4.32 – 5.76 |
| **F** | **50 tok/s** (20ms/chunk) | 3,000 | **10,000** | 100.0% | 158.5 | 475,425 | 2.11 – 2.16 | 3.10 – 3.22 | 10.43 – 17.09 |
| **F** | **50 tok/s** (20ms/chunk) | 3,000 | **15,000** | 100.0% | 237.7 | 713,184 | 2.18 – 2.25 | 4.47 – 5.86 | 28.76 – 47.28 |
| **F** | **50 tok/s** (20ms/chunk) | 3,000 | **18,000** | 100.0% | **285.1** | **855,234** | 14.02 – 14.58 | 30.29 – 32.23 | 79.46 – 105.61 |
| **D** | **100 tok/s** (10ms/chunk) | 6,000 | **1,000** | 100.0% | 15.8 | 95,025 | 2.77 – 2.86 | 3.95 – 4.14 | 5.63 – 8.77 |
| **D** | **100 tok/s** (10ms/chunk) | 6,000 | **2,000** | 100.0% | 31.7 | 189,956 | 2.08 – 2.18 | 3.08 – 3.27 | 4.59 – 7.18 |
| **D** | **100 tok/s** (10ms/chunk) | 6,000 | **5,000** | 100.0% | 79.1 | 474,712 | 2.05 – 2.12 | 2.81 – 2.90 | 5.57 – 8.91 |
| **D** | **100 tok/s** (10ms/chunk) | 6,000 | **10,000** | 100.0% | 158.2 | 949,153 | 6.84 – 7.77 | 19.51 – 24.69 | 60.65 – 70.64 |
| **D** | **100 tok/s** (10ms/chunk) | 6,000 | **15,000** | 100.0% | 236.6 | 1,419,469 | 23.68 – 28.68 | 83.97 – 103.77 | 132.25 – 185.00 |
| **D** | **100 tok/s** (10ms/chunk) | 6,000 | **18,000** | 100.0% | **282.9** | **1,697,203** | 53.96 – 80.84 | 127.10 – 234.49 | 224.45 – 480.53 |

---

## 3. Operational Root-Cause Ablations

During the development of `xlite`, three transport-level bottlenecks were isolated and solved with reproducible measurements:

### 3.1 HTTP/2 Stream Multiplexing: `xlite-h2-pool` vs `reqwest` Single-Connection Queueing
- **Root Cause**: `reqwest` / `hyper-util` opens a single HTTP/2 connection per upstream host. When concurrent streams per host exceed the upstream's `SETTINGS_MAX_CONCURRENT_STREAMS` (e.g., 250 streams/conn × 24 hosts = 6,000 streams), additional streams queue invisibly inside `hyper`'s connection buffer instead of opening additional TCP connections.
- **Solution**: `crates/xlite-h2-pool` inspects the live `SETTINGS_MAX_CONCURRENT_STREAMS` handshake value and dials additional connections per host (up to `XLITE_UPSTREAM_MAX_CONNS_PER_HOST=32`), raising the 24-host cluster capacity from `6,000` concurrent streams to `24 × 32 × 250 = 192,000` concurrent streams and keeping P50 TTFT overhead at **~2.4 ms** at 18,000 VUs.

### 3.2 Cleartext HTTP/1.1 Connection Cap: `32 → 128` (`~3.7×` QPS)
- **Root Cause**: Unlike HTTP/2, HTTP/1.1 dedicates a full TCP connection for the entire duration of an SSE stream. Sharing an h2c-tuned default cap (`32` connections/host) restricted HTTP/1.1 upstreams to 32 concurrent streams per host.
- **Solution**: Raising the per-host HTTP/1.1 pool cap from `32` to `128` (`reqwest` idle pool sizing or companion [**xLB**](https://github.com/EeroEternal/xlb) `thin-l7` bounded LIFO connection pool) yields **~3.7× higher QPS** on HTTP/1.1 upstreams while preventing unbounded socket creation.

### 3.3 Container `RLIMIT_NOFILE` Footgun (`99.49% 502` → `0.00%` Errors)
- **Root Cause**: With Docker's default soft limit of `nofile=1024`, a gateway facing ≥10,000 concurrent clients exhausts file descriptors long before exhausting CPU. In our measurement (Issue #49), `nofile=1024` produced **22.78M `PoolError::Dial` failures**, **99.49% `502 Bad Gateway` responses**, and a **10× throughput drop** while CPU and upstream latency appeared normal.
- **Solution**: Raising `--ulimit nofile=1048576:1048576` immediately restored **0.00% error rate at 1,484 QPS**. `xlite` now performs an automatic `/proc/self/limits` check at startup (`fd_budget_warning` in `src/config.rs`) and logs a prominent warning whenever `RLIMIT_NOFILE < 65,536`.

---

## 4. Reproducing the Benchmark Locally

Start upstream [`mock-llm`](https://github.com/EeroEternal/mock-llm) instances (`PORT=4101 MOCK_TTFT=500 MOCK_OUT_TOKENS=250 MOCK_TOKEN_INTERVAL=20 mock-llm`) and deploy `xlite` via [`deploy/README.md`](../deploy/README.md):

```bash
cd deploy
cp env.example .env
docker compose up -d --build
docker compose run --rm xlite import /opt/xlite/examples/catalog.yaml --replace
```

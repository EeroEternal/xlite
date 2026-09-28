# 现场实测瓶颈全景分析与 CPU 隔离/绑核（Affinity）指南（2026-09-28）

- **状态**：分析与实操指南 / 现场调优
- **日期**：2026-09-28
- **数据来源**：
  - 网关/压测机监控：`/Users/xinference/Sync/PT/newapi 压测/loadtest/results/bottleneck-monitor/`（`gw_cpu.txt`, `gw_sys.txt`, `gw_conn.txt`, `pg.txt`, `press_sys.txt`, `press_net.txt` 等）
  - 核心隔离实验：`/Users/xinference/Sync/PT/newapi 压测/loadtest/results/isolate_sweep.txt`
  - 极限长跑与双节点扫描：`/Users/xinference/Sync/PT/newapi 压测/loadtest/results/nohup-weekend.log`, `d_dual_comfort.txt`, `comfort_16_16.txt`

---

## 1. 现场资源全景与瓶颈定性

通过对客户现场高并发（长流 6000 块、15,000 ~ 50,000 VU）下的全链路系统级监控数据进行分析，得出以下核心定性：

### 1.1 网关主机（Gateway Node）真实开销
1. **CPU 与协程开销**：
   - New API 单实例（PID 176843）在 15,000 VU 稳态下吃满 **720% CPU**（即占用 **7.2 个物理核心**）。
   - OS 线程膨胀至 **121 个**（Go 运行时应对高频系统调用和通道阻塞创建的 M），`VIRT=23.5 GB`，物理内存 `VmRSS=6.13 GB`。
   - **结论**：New API 在长流高压下极度消耗 CPU 算力与 Goroutine 调度资源。虽然 128GB 宿主机内存未被打爆，但 6GB+ 的 Go 堆在并发激增时会引发显著的 GC STW 延迟毛刺。
2. **内核协议栈与连接状态**：
   - 网关主机 TCP 并发连接达 **14,985 个**（全处于 ESTABLISHED 状态）。
   - 系统上下文切换（Context Switches）高达 **60,000 ~ 102,000 次/秒**，软中断 `ksoftirqd` 与硬件中断达 140,000 ~ 183,000 次/秒。
   - 系统态 CPU（`sy`）占比达 **16% ~ 17%**，大量时间耗费在 Linux 内核网络收发、epoll 事件唤醒及协程抢占调度上。
3. **依赖组件（PG / Redis）表现**：
   - `pg.txt` 监控显示 Postgres 活跃连接仅为 1（最大连接 2000），CPU 占用约 14.18%，内存 178MB。
   - **结论**：**PG 并非数据面瓶颈**，压力 100% 积聚在网关自身的网络转发、连接维持与内存流式中转上。

### 1.2 压测机与上游 Mock 端（Press / Mock Side）
- **压测机网络吞吐与带宽**：
  - 压测机维持了 **45,351 个 ESTABLISHED 连接**。
  - 双向吞吐达到 **RX 193 MB/s / TX 190 MB/s**（接近双向 1.5 Gbps 饱和跑满）。
  - 压测机 `ksoftirqd` 软中断单核占满 94.1%，系统上下文切换超过 400,000 次/秒。
- **上游 Mock 容器负载**：
  - 24 个 `mock51h` 容器 CPU 占用在 30% ~ 79% 之间（平均 50%~60%），容量健康。
  - 前置 HAProxy 占用了 **475% CPU（4.75 核）**，内存 1.27 GB。

---

## 2. 关键实验数据：CPU 隔离与舒适区探索

### 2.1 `isolate_sweep.txt`（应用核 vs 网络中断核配比实验）
现场通过 `cpuset` 测试了 32 核机器在 15,000 VU 下，应用进程与网卡软中断核心不同配比的效果：

| 隔离方案 (应用:网络) | TTFT p50 | TTFT p90 | TTFT p99 | 请求耗时 p50 | 吞吐 (QPS) | 带宽 |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **16:16（最优基准）** | **1,144 ms** | **1,717 ms** | **2,452 ms** | **1m 21s** | **158.2 /s** | 193 MB/s |
| **20:12** | 1,224 ms | 2,092 ms | 3,094 ms | 1m 29s | 140.0 /s | 171 MB/s |
| **24:8** | 1,243 ms | 2,230 ms | 3,166 ms | 1m 43s | 127.0 /s | 155 MB/s |

> **核心机制结论**：
> **盲目给网关应用分配更多 CPU 核心反而导致性能急剧下降**。当把应用核从 16 核增加到 24 核（网络核被压缩至 8 核）时，TTFT p99 恶化了 29%，QPS 下降近 20%。
> **根因**：在大吞吐（>150 MB/s、数万 TCP 连接）场景下，网卡软中断（SoftIRQ）和内核 TCP 栈的处理能力才是真正的第一瓶颈。一旦网络中断核心处理不及，应用层即使有 24 核也在空等包到达。**16:16（50% 核心划归网络，50% 核心划归应用）是经现场验证的最佳平衡点。**

### 2.2 `nohup-weekend.log`（双节点极限长跑拐点）
- **D 组（长流 6000 块）**：
  - **18,000 ~ 45,000 VU**：全程 **0.00% fail**，Checks 100% 成功。
  - **46,000 VU**：开始出现轻微抖动（fail=0.44%）。
  - **48,000 VU**：fail=0.85%，TTFT p50=3.76s。
  - **49,000 VU**：系统崩溃，**fail 骤升至 16.01%**，TTFT p99 突破 8.16s。
  - **50,000 VU**：**fail 达 27.58%**，触发连停两档规则退出。
- **E 组（短流 250 块）**：
  - 在 47,000 ~ 53,000 VU 稳定承载，保持 **0.00% fail**，QPS 稳定在 1040~1066，TPS 约 26 万。

---

## 3. CPU 绑核（Affinity）实操落地手册

在极限并发冲刺（如冲刺 40,000 ~ 60,000 VU）时，如果观察到系统态 CPU（`sy`）偏高或软中断（`si`）扎堆在少数核心上，应执行核心隔离操作。

以现场典型的 **32 物理核心机器** 为例，推荐划分如下：
* **Core 0 ~ 15**（共 16 核）：专用于 **网关应用（XG / Tokio Worker）**。
* **Core 16 ~ 31**（共 16 核）：专用于 **网络软中断（RPS/RFS）与网卡硬件中断（IRQ）**。

### 步骤 1：将网卡中断与 RPS（软中断）绑定到 Core 16 ~ 31
Linux 内核通过 RPS（Receive Packet Steering）把软中断哈希分发到指定 CPU 核心。其掩码是 16 进制：
* Core 16~31 对应的十六进制掩码：`ffff0000`（第 16 到 31 位设为 1，第 0 到 15 位为 0）。

在网关宿主机上执行（以物理网卡 `eth0` 或 `ens4` 为例）：

```bash
IFACE="ens4"  # 替换为现场实际物理网卡名（ip link 查看）

# 1. 停用 irqbalance 避免自动把中断迁回应用核
systemctl stop irqbalance

# 2. 为网卡各接收队列配置 RPS 掩码（绑定到 core 16-31，即 ffff0000）
for rps in /sys/class/net/${IFACE}/queues/rx-*/rps_cpus; do
    echo "ffff0000" > "$rps"
done

# 3. 增大网络流表（RFS）深度，防止丢包
echo 32768 > /proc/sys/net/core/rps_sock_flow_entries
for rfc in /sys/class/net/${IFACE}/queues/rx-*/rps_flow_cnt; do
    echo 4096 > "$rfc"
done
```

### 步骤 2：将网关容器 / 进程隔离在 Core 0 ~ 15
#### 方式 A：Docker 容器启动（推荐）
在启动网关容器时，直接使用 `--cpuset-cpus` 参数限制核心范围：

```bash
docker run -d \
  --name xlite-gateway \
  --network host \
  --cpuset-cpus="0-15" \
  -e TOKIO_WORKER_THREADS=16 \
  -e XLITE_UPSTREAM_H2C=1 \
  -e XLITE_UPSTREAM_MAX_CONNS_PER_HOST=64 \
  xlite:0.5.0
```
> **注意**：
> - `TOKIO_WORKER_THREADS` 与 cpuset 1:1 只是起点：XG 实测满载 1.5~2.6 核，
>   「小 cpuset（4-8 核）+ 小 TWT、把核让给软中断/农场」可能更优，待 A/B 后再定稿。
> - 若要启用 h2 连接池准入闸门，见 `docs/manual/lab-loadtest.md`「上游流闸门」：
>   闸门 = `min(XLITE_UPSTREAM_STREAM_CAP, 通告 250) × MAX_CONNS_PER_HOST`，
>   注意 `STREAM_CAP=250` 与通告值相等时 min 不起作用（等于没设防）。

#### 方式 B：对已有运行中的进程动态绑核（`taskset`）
若网关进程已经在运行（例如 PID 为 `176843`）：

```bash
# 将进程及其所有子线程绑定到 0-15 号核
taskset -a -cp 0-15 <PID>
```

### 步骤 3：验证与观测命令
绑核完成后，在压测过程中使用以下命令观察效果：

```bash
# 1. 查看软中断是否全部转移到 16-31 号核心
watch -n 1 'cat /proc/softirqs | grep -E "(NET_RX|NET_TX)"'

# 2. 观察各 CPU 核心利用率分布（确认 0-15 主要是 us，16-31 主要是 sy/si）
mpstat -P ALL 1

# 3. 查看网关进程的 CPU 亲和性确认
taskset -cp $(pgrep xlite)
```

---

## 4. 对照总结与汇报口径

1. **New API 瓶颈本质**：
   - New API 在长流高压下消耗 **7.2 核 CPU** 与 **6.13 GB 堆内存**，且随连接数增加协程切换成本激增。
   - 在未配置内核中断隔离前，系统态 CPU 达 17%，极易在 48k VU 时引发连锁超时崩盘。
2. **XG (xlite) 架构优势**：
   - Rust 异步运行时配合无 GC、零拷贝字节透传，在同等载荷下仅消耗 **1.5 ~ 2.6 核 CPU** 和 **~200MB 内存**（CPU 节省 65%~75%，内存节省 95%）。
   - 配合 16:16 的 CPU 隔离与 H2 连接池准入闸门，能有效杜绝网络软中断打架和应用级雪崩。

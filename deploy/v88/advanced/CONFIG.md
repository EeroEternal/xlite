# 现场配置文档（V88 框架）

面向**去客户现场部署 + 压测**的一份配置口径。环境变量逐个与 `src/config.rs` 核对过；
数值与判读纪律以 [`docs/manual/v88-rig.md`](../../../docs/manual/v88-rig.md) 与
[`docs/dev/loadtest-newapi-controlled-2026-09.md`](../../../docs/dev/loadtest-newapi-controlled-2026-09.md) 为准。

> 本文只讲**怎么配、为什么这么配**。怎么跑压测见 [`README.md`](README.md)。

---

## 0. 一页总览（照这个填）

| 位置 | 项 | 值 | 依据 |
| --- | --- | --- | --- |
| 宿主机（每台网关机） | `net.core.somaxconn` | `65535` | backlog 是并发上限杠杆 |
| 宿主机 | `net.core.netdev_max_backlog` | `65535` | 同上 |
| 宿主机 | `net.ipv4.tcp_max_syn_backlog` | `65535` | 同上 |
| 宿主机 | `net.ipv4.ip_local_port_range` | `1024 65535` | 出站端口够用 |
| 宿主机 | `net.ipv4.tcp_tw_reuse` | `1` | 短连接回收 |
| 容器 `xlite` | `ulimit nofile` | `1048576`（soft=hard） | 封顶时吞吐静默掉到 1/4 |
| 容器 `xlite` | `TOKIO_WORKER_THREADS` | **`8`**（1000 流级别可试 `2`） | §1 |
| 容器 `xlite` | `XLITE_UPSTREAM_H2C` | `1`（上游是明文 h2c 时） | — |
| 容器 `xlite` | `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` | **`32`（别动）** | 32/64/128 实测零效应 |
| 容器 `xlite` | `XLITE_UPSTREAM_STREAM_CAP` | **不设**（或 ≥ 对端通告值；对端常为 250） | §5.1：低于对端值会把中位首字换成尾部 |
| 容器 `xlite` | `XLITE_MAX_IN_FLIGHT` | **不要设** | 已 revert；只把排队搬进 TTFT |
| 容器 `xlite` | `ADMIN_PASSWORD` / `ADMIN_SESSION_SECRET` | 自己定；**多节点必须一致** | 无状态签名 Cookie |
| HAProxy | `option httpchk` | **`GET /health`** | 客户现在是 `/api/status`（New API 的） |
| HAProxy | `timeout client/server/tunnel` | `600s` | 客户已是对的，别改小 |
| HAProxy | 粘性 / `stick-table` | **不需要** | 会话是签名 Cookie，任意节点可校验 |
| k6 | `STREAM_TIMEOUT` | `600000` | 与上游 reqwest 600s 对齐 |
| k6 | `KEEP_BODY` | 对标 `true`（配 `KEEP_SAMPLE=1`）/ 产能 `false` | 两套试验必须分开跑，见 [`README.md`](README.md) |
| mock 农场 | `MOCK_BATCH` | 对标 `1` / 产能 `5`·`10` | 口径，不是性能旋钮 |

---

## 1. `TOKIO_WORKER_THREADS` 怎么设

### 1.1 为什么必须显式设

裸 `#[tokio::main]` 在环境变量缺席时**按 `nproc` 起 worker**。客户网关机核多，默认值等于
「有多少核就起多少 worker」——而这是个 **I/O 密集型**网关，实测只要 1.6–2.7 核就够。
多余的 worker 全是纯开销：work-stealing、跨核唤醒、缓存行反弹（机制为**推断**，未直接测）。

**这条是"白烧 CPU 且尾部更差"，不是"多花钱买保险"。**

### 1.2 实测（1000 VU 平推 120s，单臂 A2，128 核机）

| `TOKIO_WORKER_THREADS` | cores | p50 (ms) | p90 | p95 | p99 | qps | fail |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 默认（= nproc = **128**） | 2.670 | 0.5498 | 1.8454 | 3.5039 | 7.7366 | 154.67 | 0.00% |
| 32 | 2.607 | 0.5153 | 1.5688 | 3.1201 | 6.2772 | 154.69 | 0.00% |
| **8** | 2.449 | 0.5414 | 1.6502 | 2.9072 | 6.2451 | 154.73 | 0.00% |
| **2** | **1.579** | 0.6281 | **1.5301** | **1.9293** | **3.7758** | 154.80 | 0.00% |

- **128 → 2 省 40.9% CPU**，同时 `p90 −17%` / `p95 −45%` / `p99 −51%`，代价 `p50 +14%`。
  ⇒ **不是拿 CPU 换延迟，是两头都变好**（p50 的 +0.078ms 相对 500ms 的首字可忽略）。
- **吞吐与失败率零变化** —— 这些线程在 1000 路 / ~155 req/s 下本来就不是瓶颈。

4000 VU 下幅度变小：**128 → 8 省 9%**（2.563 → 2.330 核）。

### 1.3 怎么选（决策规则）

```
按**活跃流规模**定，不抄常数：
  ~1000 流级        → 2 或 4（2 是实测最优；但见下方"未测边界"）
  4000 流级以上     → 8（唯一在 1000 与 4000 两档都实测过且都最好的值）
  不确定            → 8（deploy/env.example 与 compose 的默认值）
```

**幅度随载荷变小**：低载时固定开销占大头，所以省得多（−40.9%）；满负荷时真实并行度上去了，
省得少（−9%）。**别把一个档位的最优值抄到另一个档位。**

> ⚠️ **未测边界（别当成已否定）**：4000 VU 只测过 `128 / 32 / 8`，**8 最好，但 4 与 2 没试过**，
> 拐点在哪不知道。往上打档位时如果要重调，按 §1.5 的办法自己扫。

### 1.4 怎么设（四种路径，任选）

**① Docker Compose（推荐，与 [`../../docker-compose.yml`](../../docker-compose.yml) 一致）**

`deploy/.env` 里写：

```sh
TOKIO_WORKER_THREADS=8
```

compose 里是 `${TOKIO_WORKER_THREADS:-8}`，所以**不写 `.env` 也会取 8**（默认已经是 8，不是 nproc）。

**② 单容器 `docker run`**

```sh
docker run -d --name xlite-gw --network host \
  --ulimit nofile=1048576:1048576 \
  -e TOKIO_WORKER_THREADS=8 \
  -e REDIS_URL=redis://127.0.0.1:6379 \
  -e XLITE_UPSTREAM_H2C=1 \
  -e ADMIN_PASSWORD='<自己定>' \
  -e LISTEN_HOST=0.0.0.0 -e PORT=8006 \
  xlite:0.5.3
```

**③ 裸机 / systemd**

```ini
[Service]
Environment=TOKIO_WORKER_THREADS=8
```

**④ 压测脚本里临时覆盖**：`ab_topology.sh` 的 `TWT=`、`ab_configs.sh` 的 `CONFIGS="TWT=8"`。

### 1.5 怎么验证**真的生效了**

⚠️ **网关不会打印 worker 线程数**（`src/main.rs` 里没有这行日志），所以**没有日志可回读**。
两条可行的核对：

```sh
# (a) 线程数增量 —— 只能当"粗略指示"，不是精确回读
#     （/proc/<pid>/task 里除了 tokio worker 还有 blocking pool 等线程）
docker exec xlite-gw sh -c 'ls /proc/1/task | wc -l'
#     在同一台机上改 TWT 重启，比较这个数的**差值**，方向应与 TWT 一致

# (b) 权威做法：固定负载下比 CPU（这正是当初得出 §1.2 的方法）
#     采样网关进程 CPU：utime+stime 增量 ÷ 墙钟
pid=$(docker inspect -f '{{.State.Pid}}' xlite-gw)
awk -v p="$pid" '{print systime(), $14, $15}' /proc/$pid/stat   # 压测期间反复采
```

> 本仓压测记录里必须记下 `TOKIO_WORKER_THREADS` 的**生效值**（`v88-rig.md` §1.3 的报告契约
> 要求每个结果目录自带它）—— 只记"我设了"不算凭据。

### 1.6 与 `nofile` / `backlog` 的关系

**调线程不会影响 backlog 悬崖**：4000 VU 下 `TWT=128/32/8` 的失败率都是 **34.6%**，
失败**绝对计数**也几乎相同（12 422/12 430/12 433）——这是纯 backlog 属性。
但两个旋钮要**一起上**，方向是**先 `nofile` 后 `backlog`**：只调 backlog 会得到
「不报错但只有 1/4 吞吐」这种更难察觉的形态。

---

## 2. HA 配置（HAProxy 双节点）

### 2.1 客户现状

客户包里那份是 `bench/0924/deploy_origin/deploy88/haproxy.cfg`（**客户包在 `bench/` 下、被
`.gitignore` 忽略、不进仓库**，所以这里只是文献引用 —— 与
[`docs/manual/v88-rig.md`](../../../docs/manual/v88-rig.md) 引用客户包的方式一致）：
`:8005` 双节点（32+33）、`:8006` 单节点（仅 32），`httpchk GET /api/status`，
`timeout client/server/tunnel 600s`，`mode http`，`option http-keep-alive`。

### 2.2 从客户包那份 cfg（New API 的）起步时要动的**三处**

| # | 客户包那份 | 改成 | 不改的后果 |
| --- | --- | --- | --- |
| 1 | `option httpchk GET /api/status` | **`GET /health`** —— **已经探 `/health` 就不用动** | xlite 没有 `/api/status` → 探活一直失败 → **节点被摘掉** |
| 2 | `server node1 10.56.88.32:8001` | 指向 xlite 监听端口（现场是 `server n24 10.56.88.24:8080` / `n26 …:8080`） | — |
| 3 | （无粘性配置） | **保持无粘性** | 无状态签名 Cookie，加粘性只是白搭还降低可用性 |

超时**不用动**（客户已是 600s）。`maxconn 200000`、`option http-keep-alive` 也保持。

### 2.3 可直接用的一份

```haproxy
global
    maxconn 200000
    log stdout format raw local0

defaults
    mode http
    log global
    option http-keep-alive
    timeout connect 10s
    timeout client 600s          # SSE 长流：别改小
    timeout server 600s
    timeout tunnel 600s          # 隧道/长连接阶段
    timeout http-request 60s     # 只约束"收完请求"这一段

frontend api_dual
    bind *:8005
    default_backend xgw_dual

frontend api_single
    bind *:8006
    default_backend xgw_single

backend xgw_dual
    balance roundrobin
    option httpchk GET /health
    http-check expect status 200
    server n24 10.56.88.24:8080 check inter 5s fall 5 rise 2
    server n26 10.56.88.26:8080 check inter 5s fall 5 rise 2

backend xgw_single
    balance roundrobin
    option httpchk GET /health
    http-check expect status 200
    server n24 10.56.88.24:8080 check inter 5s fall 5 rise 2
```

**可选增强**（未在本仓测过，按需自行评估，别当既有结论）：

```
    option redispatch            # 连接失败时改投另一台
    retries 3
```

### 2.4 为什么是这几个值

| 设置 | 理由 |
| --- | --- |
| `httpchk GET /health` | 免鉴权、只判状态码；xlite 的 `/health` 还回 `version`，便于远程核对现场跑的是哪一版 |
| `timeout tunnel 600s` | 与 k6 `STREAM_TIMEOUT=600000`、xlite 上游 reqwest 600s **三处对齐**；SSE 长流不能在这三处任何一处被砍 |
| `timeout client/server 600s` | 与上面同源 |
| 不设粘性 | Admin 会话是无状态 HMAC 签名 Cookie（`xlite_session`），任意节点独立校验；HAProxy 轮询下登录态共享 |
| `mode http` | 客户端侧是 HTTP/1.1；网关到上游另走 h2c，与 HAProxy 无关 |

### 2.5 不要做的

- **不要用 429 挡过载**。xlite 数据面不对客户端限流：只给 401/400/404/502；
  上游 429 会触发**换通道**而不是回给客户端。用 HAProxy 限流会把「排队」变成「拒绝」，
  制造一份看起来更差、且与两侧不同口径的对标数据。
- **不要给 HAProxy 加更短的超时**。过载时该变长的是 duration/TTFT，不是失败率。
- **不要靠 HAProxy 做缓冲来"优化" TTFT**。`option http-buffer-request` 只影响**请求**；
  任何在中间层攒响应体的做法都会把首字推迟 —— 那正是我们不允许的那类"优化"。
- **不要设 `XLITE_MAX_IN_FLIGHT`**（见 §5）。

### 2.6 验证

```sh
# 1) 探活路径对：两个节点都应 200，且 body 里有 version
curl -sS http://<gw1>:8001/health   # {"status":"ok","gateway":"xlite","version":"..."} 
curl -sS http://<gw2>:8002/health

# 2) HAProxy 视角：两台都不该是 DOWN
#    （haproxy 2.9 起可用 stats socket；没有就靠 8005/8006 的实际轮询）
for i in 1 2 3 4; do curl -sS http://<入口>:8005/health; echo; done   # 交替也要都 200

# 3) SSE 能穿过 HAProxy（不是只过 /health）
curl -N -sS -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model":"mock-model","stream":true,"messages":[{"role":"user","content":"hi"}]}' \
  http://<入口>:8006/v1/chat/completions
```

> **客户现场那 ~205ms 要单独定位**：V88 日志里 New API `ttft p50 = 708ms` 而
> `gw_oh p50 = −205ms` —— 这两者是**同一个延迟的两面**（网关把客户端可见的流起点推迟 D，
> 则 TTFT `+D`、`receiving` `−D`）。同一镜像在实验台上只有 `+4.2ms`，所以那 ~205ms 来自
> **现场的路径（HAProxy 跳 / 网络）**，不是网关实现。把它算进"网关开销"是错的，
> 但也**不能假定它对两侧同加**（§10.2 已标注这是未验证推论）。

---

## 3. 内核与 fd（链路上**每一环**都要查：HAProxy / 网关 / mock / k6）

```sh
# 客户包里已有一份：bench/0924/deploy_origin/deploy88/sysctl-loadtest.conf
cp bench/0924/deploy_origin/deploy88/sysctl-loadtest.conf /etc/sysctl.d/99-loadtest.conf
sysctl --system
```

要点（其余见该文件）：`net.core.somaxconn=65535`、`net.core.netdev_max_backlog=65535`、
`net.ipv4.tcp_max_syn_backlog=65535`、`net.ipv4.ip_local_port_range="1024 65535"`、
`net.ipv4.tcp_tw_reuse=1`、`net.ipv4.tcp_fastopen=3`。

**为什么 `somaxconn` 必须设**：网关进程的 listen backlog 已经是 65535（`src/main.rs`），
但**内核按 `net.core.somaxconn` 封顶**。libc `SOMAXCONN=128` 那个坑代码已经绕过，
`somaxconn` 这一层绕不过去。

容器：

```sh
--ulimit nofile=1048576:1048576      # soft = hard，别只改 soft
# 或 compose：ulimits: { nofile: { soft: 1048576, hard: 1048576 } }（deploy/ 已写死）
```

核对（**每台都查**）：

```sh
docker compose exec xlite sh -c "grep -i 'open files' /proc/1/limits"   # 期望 1048576
sysctl -n net.core.somaxconn                                          # 期望 65535
```

### 3.1 一次查完链路上的七处上限

`maxconn` / `--ulimit` / `sysctl` 分布在**四台机**上，任何一层小一个量级都会造出**同一种签名**：RST 暴涨、`dial: i/o timeout`，或者吞吐静默掉一截而 `fail` 与延迟指标全都正常。

**网关容器这一层（②）的签名最阴**：2026-09-30 实验室实测（issue #49）——`nofile` 被封在 1024 时它**不报错**，而是**成片 502 且吞吐掉到约 1/10**：池内 2278 万次 `PoolError::Dial` 失败、99.49% 的请求 502、qps 约 155；把 fd 补齐后**同一档** `fail=0.00%`、qps 1,484。而这段时间 `fail` / `vus_max` / `iteration_duration` **全部正常**。⇒「`fail` 是 0，所以环境没问题」这个推理**不成立**，必须按下面这张表逐环核。

| # | 在哪台 | 上限 | 期望 |
| --- | --- | --- | --- |
| 1 | 22 | **HAProxy 进程 `nofile`**（`maxconn` 的兑现能力，见 §3.2） | ≥ 210000 |
| 2 | 24 / 26 | 网关容器 `nofile` | 1048576 |
| 3 | 29 | **k6 容器 `nofile`** —— driver 已代设，手工起 k6 才要自己加 `--ulimit` | 1048576 |
| 4 | 22 / 29 | `nf_conntrack_max`（表满即丢包 → RST / 超时） | ≥ 2~4× 峰值连接数 |
| 5 | 22 | `somaxconn` / `tcp_max_syn_backlog`（SYN / accept 队列） | 65535 |
| 6 | 29 | `ip_local_port_range` / `tcp_tw_reuse`（k6 的**出站**端口） | `1024 65535` / `1` |
| 7 | 29 | **24 口 mock 容器 `nofile`** —— 每口要持有「在途请求数 ÷ 口数」条**入站**连接；最坏情况按「全部 VU 同时占连接」估：现场 55000 ÷ 24 ≈ **2,292** | 1048576（**1024 / 4096 这类默认值会先把农场压垮**，且它的症状和 ④ 的表满很难区分） |
| + | 全部 | `systemctl show docker -p LimitNOFILE` —— 它封顶**任何**容器能拿到的上限，所以上面 2 / 3 / 7 三行不可能超过它 | 1048576 |

```sh
hc=$(pgrep -f haproxy | head -1)                                    # 22
grep -i 'open files' /proc/$hc/limits                               # ①
docker exec xlite-gw sh -c "grep -i 'open files' /proc/1/limits"    # ②（24 / 26）
k6p=$(pgrep -f ' k6$' | head -1)                                    # 29
grep -i 'open files' /proc/$k6p/limits                              # ③
sysctl net.netfilter.nf_conntrack_count net.netfilter.nf_conntrack_max   # ④
sysctl net.core.somaxconn net.ipv4.tcp_max_syn_backlog              # ⑤
sysctl net.ipv4.ip_local_port_range net.ipv4.tcp_tw_reuse           # ⑥
for c in $(docker ps --filter name=mock --format '{{.Names}}'); do   # 29：24 口农场
  echo -n "$c "; docker exec "$c" sh -c "grep -i 'open files' /proc/1/limits"
done                                                                 # ⑦
```

`docker inspect` 看的是容器**被要求**的上限（≠ 生效值，后者以 `/proc/1/limits` 为准）：

```sh
docker inspect xlite-gw --format '{{.HostConfig.Ulimits}}'           # ②：要求了什么
docker inspect xlite-gw --format '{{.State.Pid}}'                    #    再 grep 它的 /proc/<pid>/limits 才是生效值
```

**先测再改**：已经是指望值的就别动它 —— 改不必要的变量只会让下一轮更难归因。只有看到 1024 / 4096 / 65536 这种量级才改。

### 3.2 HAProxy 的 `maxconn` 必须与它的 fd 上限对账

`global maxconn 200000` 只是 HAProxy 的**内部计数**；真正的 fd 上限由**启动它的环境**给（systemd `LimitNOFILE` / 容器 `--ulimit` / 父 shell 的 `ulimit -Hn`）。HAProxy 默认会按 `maxconn` **自动算出**所需 fd 数并自己 `setrlimit` 去抬（等价于配置里的 `global ulimit-n`，所以 §2.3 那份 cfg **不需要**额外写它），**但抬不过 hard 上限**；抬不上去时它只在启动日志里留一条 warning，**不影响启动**。

所以判据是运行时的数，而不是配置文件：

```sh
pid=$(pgrep -f haproxy | head -1)
grep -i 'open files' /proc/$pid/limits                                    # soft 是它真正拿到的
ls /proc/$pid/fd | wc -l                                                  # 此刻已用
journalctl -u haproxy -b --no-pager | grep -iE 'ulimit|open files' | tail -3
# 开了 stats socket 之后（§2.6 第 2 条）最权威：
echo 'show info' | socat stdio /run/haproxy/admin.sock | grep -iE 'Maxconn|Maxsock|CurrConns'
#   Maxsock = HAProxy 按 maxconn 算出的需求；它 > soft 就是被封顶了
```

**`maxconn 200000` 却只拿到 1024 / 4096 / 65536 fd 时会发生什么**：已接受的连接持不住（客户端看到 **RST**）、SYN 接不住（→ **`dial: i/o timeout`**），而 HAProxy 自己不报错。入口机有 128G，给 21 万 fd 毫无代价（内核侧每 socket 几 KB）：

```sh
mkdir -p /etc/systemd/system/haproxy.service.d
printf '[Service]\nLimitNOFILE=210000\n' > /etc/systemd/system/haproxy.service.d/nofile.conf
systemctl daemon-reload && systemctl restart haproxy    # ★ reload 不重读 LimitNOFILE
```

**对标时的披露要求**：两侧各自的入口 HAProxy 都要按 §3.1 核同一张表；只调一侧**必须在报告里写明** —— 这是**基础设施不对称**，不是口径造假，但不说清就没法解释数字差。

---

## 4. 两节点与目录

| 项 | 做法 |
| --- | --- |
| 目录存储 | 集群用 **Redis**（`REDIS_URL=redis://…`，键 `xlite:catalog`）；有 `REDIS_URL` 时**不要**再设 `DATABASE_URL` |
| 节点间一致性 | 各节点最多约 5 秒后与 Redis 一致（`XLITE_CATALOG_RELOAD_SECS`，默认 5；压测可设 `0` 冻结快照） |
| Admin 登录 | `ADMIN_PASSWORD`（或显式 `ADMIN_SESSION_SECRET`）**各节点必须一致**；会话是签名 Cookie，无需粘性 |
| 目录导入 | `docker compose run --rm xlite import <yaml> --replace`；**不要**把 `examples/mock-24.yaml`（`127.0.0.1`）导进容器网关 |
| 端口 | 容器内 `PORT`；宿主机映射或 `--network host` |

### 4.1 空 Redis 时怎么灌目录

**网关没有目录时每个请求都回 401**（启动不会自动灌入）；现场那台 Redis 里已经有目录的话，
`redis-cli -h 10.56.88.22 get xlite:catalog` 有内容就跳过本节。

生成 24 条 upstream 指向 mock 机的目录 YAML（`host` 改成你的 mock 机；现场 mock 在 `:8081-8104`）：

```sh
python3 - <<'PY' > /tmp/mock-24.custom.yaml
host = "10.56.88.29"          # ← mock 机
ports = range(8081, 8105)
print("providers:")
for p in ports:
    print(f"  - name: mock-{p}")
    print(f"    base_url: http://{host}:{p}")
    print('    api_key: ""')
    print("    enabled: true")
print("models:")
print("  - name: mock")
print("    enabled: true")
print("    channels:")
for p in ports:
    print(f"      - provider: mock-{p}")
    print("        upstream_model: mock")   # mock 的模型 id
    print("        weight: 1")
    print("        priority: 0")
    print("        enabled: true")
print("api_keys:")
print("  - name: loadtest")
print("    key: sk-loadtest")
print("    models: []")
print("    enabled: true")
PY
```

导入（在能连到那台 Redis 的机器上跑；镜像换成本机实际 tag）：

```sh
docker run --rm --network host \
  -e REDIS_URL='redis://10.56.88.22:6379' \
  -v /tmp/mock-24.custom.yaml:/tmp/cat.yaml:ro \
  xlite:0.5.3 import /tmp/cat.yaml
```

⚠️ `--replace` 是**先清空整表再导**（已有的 provider / model / key 全没）——只想合并就别加它。
导入后核对：`docker exec redis redis-cli get xlite:catalog | grep -c mock-` → 期望 `24`。

---

## 5. 不要设的东西

| 项 | 为什么 |
| --- | --- |
| `XLITE_MAX_IN_FLIGHT` | 已于 `21fa7a7` 从主干拿掉；**0.3.0 的 `src/config.rs` 根本没有这个变量，设了也不读**。它只把难看从 duration 搬到 TTFT（31000 VU 下 TTFT p50 1.4s → 27.7s） |
| `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` 调大 | 32/64/128 实测**零效应**（h2c 把多流复用到一条连接） |
| `XLITE_UPSTREAM_STREAM_CAP` 设成**低于对端通告值** | 见 §5.1：它只能把每连接并发压小（`min(对端, CAP)`），实测把中位首字换尾部；现场若设成 30，45000 档约多付 250ms |
| `XLITE_UPSTREAM_TTFT_HEADER=1` | 诊断用；开了会**等上游第一个 body 字节才发响应头 ⇒ 推迟客户端首字**。对标跑数时必须关 |
| 任何"提前写 200 / SSE 空头" | 伪造 TTFT：报表 TTFT 能低到 ~1ms（**低于 mock 的 500ms**），且破坏 failover 与状态码透传，违反 [`docs/ai/agents/wire-boundary.md`](../../../docs/ai/agents/wire-boundary.md) |
| HAProxy 侧限流 / 短超时 | 见 §2.5 |

### 5.1 `XLITE_UPSTREAM_STREAM_CAP`：唯一正确的用法是**不设**（或 ≥ 对端通告值）

它**不是提速旋钮**，而是客户端自己加的**每连接并发流上限**，与对端通告值取小：

```
每连接实际可用流数 = min(对端 SETTINGS_MAX_CONCURRENT_STREAMS, CAP)
```

⇒ 它**只能往下压**。对端是 250（那套 mock 农场的实测通告值）时：设 `250`、设 `1000`、**不设** —— 三者行为**完全一致**（设 250 与不设的首字实测逐位相同：`549.4` vs `549.5 / 549.0ms`，这也是一次内部一致性检验）；对端若什么都不通告，xlite 的兜底值同样是 250。**所以「> 250」没有意义，「< 250」才有代价。**

代价长什么样（2026-09-30 实验室：24 口农场 / 16000 VU / 60s / `MAX_CONNS_PER_HOST=25`（与现场同值）；每档都通过「`POOL_ERR=0` + 单流预检 `200`」可比性闸门）：

| CAP | ttft p50 | ttft p90 | dur p50 | dur p90 | iter/s | fail |
| --- | --- | --- | --- | --- | --- | --- |
| **30** | 771.4 / **767.0** | 1042 / 1028 | 6.97 / 6.99s | **10.04 / 10.05s** | **1702 / 1699** | 0% |
| 60 | **650.0** | 866.8 | 6.67s | 11.19s | 1668 | 0% |
| 125 | **580.0 / 584.8** | 782 / 787 | **6.33 / 6.49s** | 12.26 / **14.01s** | 1637 / **1522** | 0% |
| 250 / 不设 | **549.3 / 547.9** | 750 / 749 | 7.32 / 7.35s | 12.24 / 12.36s | 1575 / 1564 | 0% |

（每档两轮并排。30 / 250 两档重复性 **±0.5%**；125 档的尾部与吞吐**不稳**，见第 3 条。）

1. **首字随 CAP 单调变好**：30 → 767、60 → 650、125 → 580、**250/不设 → 548ms**。降 CAP 就是拿中位首字去换。
2. **它不是免费的**：CAP=30 的吞吐最高（**+8%** vs 不设）且**尾部最好**（dur p90 10.0s vs 12.2s）⇒ 「低 CAP 伤首字、护尾部与吞吐」，方向确定、量级如上。
3. **125 是纸面上的折中，但有余量陷阱**：它的 dur p50 最好（~6.4s），可**两轮不稳**（dur p90 12.26 ↔ 14.01s、iter/s 1637 ↔ 1522）。原因是在途约 358/口 时需要 ⌈358/125⌉ = 3 条连接 = 375 槽（**余量仅 5%**），一有抖动就要拨号；CAP=250 是 2 条 = 500 槽（40% 余量）⇒ 稳。**别只看单跑，也别只看 p50。**
4. **与 NA 的口径关系**：New API（Go `net/http2`）**没有这个客户端侧上限**，它跟随对端通告 ⇒ 约等于「不设」。所以*把 CAP 设成 30 再去比首字，等于单方面给自己上了个闸门*；反过来，放开 CAP 会**降低** qps（多占并发换延迟）—— 这是取舍，不是单赢。XG 在 45000 档 qps `1179` vs NA `779` 而 ttft/dur 更差，正是同一件事的两面（Little's law：在途 `10,776` vs `5,135`）。

**现场怎么查、怎么改**（`.24` / `.26` **各跑一次**，取更紧的那次）：

> **队列一直都在，只是原来看不见**：`acquire` 在**上界处**就是等（`xlite-h2-pool` 的
> 「at the cap it waits」）—— 阈值 = `min(CAP, 对端) × CONNS`。所以这不是"要不要加队列"，
> 而是"阈值定多少 / 等待怎么办"。现在 `GET /api/pool` 每 host 直接给出队列状态：
> `waiting`（当前排队数）、`waiting_max`（水位）、`queued_total`（排过队的请求数）、
> `wait_ms_mean` / `wait_ms_max` / `wait_ms_p50` / `wait_ms_p99`（分位数取自桶上界 ⇒ 是**上界**，不会低报）。
> ⚠️ 队列**不创造产能**：持续过载时它只把 `fail` 换成**等待**（代价记在 TTFT/dur 上）。
>
> **上界现在可读，不用再手算**：`GET /api/pool`（0.5.2 起）每 host 给出 `peer_cap` /
> `effective_streams_per_conn` / **`effective_ceiling`**（= `min(CAP, 对端) × CONNS`），启动时每个 host
> 也会打一行 `upstream admission ceiling … ceiling=N`；`advanced/pool_sizing_probe.sh` 直接读它。
> 这正是为了一件常被忽略的事：**调 `CAP` 会悄悄改掉总容量**（30 → 不设，上界 750 → 6,250 ✗），
> 现在它会被**打出来**，想保住 750 时数字直接告诉你 `CONNS` 该设几。
>
> 别照搬本文的实验值：`CAP` 只在**真的绑定**时才有代价。用只读探针
> [`pool_sizing_probe.sh`](pool_sizing_probe.sh)（压测**进行中**在网关机上跑）量出
> 「每口连接数 / 每口在途流数」，它会直接给出该不该改、以及 `MAX_CONNS` 该设几，
> 并附一段可贴进报告的「口径变更」说明。
> **0.5.2 起推荐读 `/api/pool`**（探针给了管理员密码就走这条路）：每条 host
> 的连接数与在途流数是**精确值**，不需要 `ss`/`docker`，读不到再自动回落 `ss`。
> 密码用 `ADMIN_PASSWORD` 环境变量传入（脚本优先读它）；`--admin-password` 会把明文留在 argv。

```sh
docker logs <gw> 2>&1 | head -3 | grep -o 'stream_cap=[^ ]*'         # 期望 "peer"（= 未设）
docker inspect <gw> --format '{{.Config.Env}}' | tr ' ' '\n' | grep STREAM_CAP
# 改法：去掉该环境变量（或设成 ≥ 对端值）后 **重建容器**
# ⚠️ `docker restart` 不会改 env，必须按原命令 / compose 重建
```

**引用时请用偏移量，不要搬绝对值**：上表是**一台** 128 核机上 24 口农场 + 同机 loopback 的实测（两轮）；客户环境多一层 HAProxy、两台网关，绝对值必有偏差。可安全引用的形式：「**放开 CAP ⇒ ttft p50 −190~220ms、dur p50 ±0.4s、dur p90 +2s、qps −4~8%**」。

⚠️ **客户环境未验证**：本节全部结论来自实验室台架。**现场 A/B 没有做过**（客户测试环境已不可用）⇒ 上表右列那些数字是**外推预测**，不是现场实测值。在任何对标报告里引用时，必须写成「预计」并注明「实验室台架外推，客户环境未验证」——**不要**写成「现场实测」或「验证过」。

**交叉验证**：`MAX_CONNS_PER_HOST` 的「零效应」（上表同 §5）在本轮被再次验证 —— 同为 CAP=30，`MAX_CONNS=25` 与 `128` 的首字分别 `771/767ms` 与 `782/772ms`，无系统差异。

---

## 6. 上线核对清单

```sh
# 版本与身份
curl -sS http://<节点>/health          # 期望 {"status":"ok","gateway":"xlite","version":"<本轮 tag，如 0.5.3>"}
# 目录后端通了
curl -sS http://<节点>/ready           # 期望 {"status":"ready"}；503 = Redis 不通
# 鉴权与模型
curl -sS -H "Authorization: Bearer $KEY" http://<节点>/v1/models
# 流式真的在流（不是攒完才吐）
curl -N -sS -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model":"mock-model","stream":true,"messages":[{"role":"user","content":"hi"}]}' \
  http://<节点>/v1/chat/completions
# 资源上限
docker exec <gw> sh -c "grep -i 'open files' /proc/1/limits"
sysctl -n net.core.somaxconn
```

四项都对得上再开压。**压测前先跑** `deploy/v88/run_full_88_xg.sh --preflight-only`。

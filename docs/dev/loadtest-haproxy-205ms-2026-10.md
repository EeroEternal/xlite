# HAProxy 跳的 ~205ms 首字延迟：归因、对照口径与 AI 流式网关的代理配置（2026-10-13）

- **现象**：对照数据里，同一网关经过 HAProxy 时 `ttft p50 ≈ 708ms`、`gateway_overhead_ms p50 ≈ −205ms`；直连网关时 `ttft p50 ≈ 503ms`、`gw_oh ≈ 0`。整段时长、QPS、token 间隔三者全程不变。
- **数据**：
  - 基线（经 HAProxy，双节点 `:8005`）：`/Users/xinference/Sync/PT/pk/xg/20261006T112016Z`（xlite，记为 **A**）
  - New API 10:12（**经 HAProxy `:8006`**，记为 **B**）：`/Users/xinference/Sync/PT/pk/1013/retest-single`
  - New API 10:49（**直连 `.32:8001`**，记为 **C**）：`/Users/xinference/Sync/PT/pk/1013/retest-single-direct`
- **拓扑**：k6 → HAProxy `.31:8005/:8006` → 网关 `.32:8001` / `.33:8002`；24 口 mock；B 组 = `out=500`、`token_interval=10ms`、30s 爬坡 + 300s 稳态、`KEEP_BODY=true`。
- **指标口径**（[`scripts/loadtest/k6/k6.js`](../../scripts/loadtest/k6/k6.js)）：`ttft_ms = res.timings.waiting`（第 174 行）；`gateway_overhead_ms = res.timings.receiving − (末块 ts − 首块 ts)`（第 82、218 行）。`ttft` 只看首字节，`gw_oh` 拿 mock 自己打的时间戳跨度做基准。

## 0. 结论速览

1. **那 ~205ms 在 HAProxy 这一跳上，不在网关转发里。** 同一 k6、同一 mock、同一 `KEEP_BODY=true`，只要绕开 HAProxy，首字就回到 ~503ms。
2. **它不能算成网关（New API 或 xlite）的性能提升。** 10:12 那次**仍然经过 HAProxy `:8006`**，首字就已经是 502.6ms —— 说明这 205ms 在 10:12 时已从路径上消失，而整段（5.87s）、QPS（161）、chunk 间隔（11ms）统统一动未动。
3. **xg 要降到 ~500ms：压测时 k6 直连网关，或把 HAProxy 配成流式友好。** 两者都行，但**不能只给一侧做**，否则 205ms 会变成一个假的胜负差。
4. **HAProxy 不是不适合 AI 网关**，是通用 L7 反向代理的**默认配置**与「流式 POST + 长命 SSE」相冲；把几个开关显式配好即可（§4）。

## 1. 证据：三组数字来自同一条时间线

B 组 1000 VU 的 k6 汇总（同批 `KEEP_BODY=true`）：

| 运行 | 入口 | ttft p50 | `gw_oh` p50 | dur p50 | qps | chunk p50 |
|---|---|---:|---:|---:|---:|---:|
| **A** 10-06（xlite） | 经 HAProxy `:8005` | 706.6ms | **−205.2ms** | 5.89s | 160.6 | 11 |
| **B** 10-12（New API） | **经 HAProxy `:8006`** | **502.6ms** | **+0.217ms** | 5.87s | 161.1 | 11 |
| **C** 10-49（New API） | **直连 `.32:8001`** | 502.5ms | +0.222ms | 5.87s | 161.1 | 11 |

数据来源：`xg/20261006T112016Z/B_1000.summary.json`、`1013/retest-single/B_1000_300s.log`、`1013/retest-single-direct/B_1000_300s.log`。

**为什么 `gw_oh = −205ms` 与 `ttft = +205ms` 是同一个延迟的两面**（[`loadtest-newapi-controlled-2026-09.md`](loadtest-newapi-controlled-2026-09.md) §10.2 已经写过）：k6 的 `receiving` 从「收到第一个字节」开始计时。若某一跳把客户端可见的流起点推迟 `D`，则 `ttft +D`、`receiving −D`，而 `ttft + receiving`（总时长）不变——这正是「整段 5.87s 不变、只有首字变」的签名。mock 的 500 帧时间戳跨度是固定的，所以 `gw_oh` 会把这个 `D` 反号显示出来。

同一现象在 New API 侧的旧数据也一致：1006 文档 §2 表里 S3/S4（New API，经 HAProxy）B 组 `ttft p50 = 708ms`，与 xlite S1 的 707ms 收敛在同一量级；两家的差别不在实现，而在「经不经那一跳」。

## 2. 收口 10:12：它的 TARGET_URL 是经 HAProxy

10:12 的目录此前只留了日志、没有 `env.txt`，所以无法从数据文件直接看到目标地址。实际发起脚本保存在测试机（也在本地 `Downloads` 与微信下载目录里留了副本），逐字节相同，SHA256 `bd617bf45496b3301a506060a657ac44f6e50a1fdb1557594892601edc7b1ff9`：

```bash
# /Users/xinference/Downloads/deploy88/loadtest/retest_single.sh（第 3、5 行）
# 目标: :8006 单节点(仅32)
TARGET='http://10.56.88.31:8006/v1/chat/completions'
```

对照的直连补测脚本：

```bash
# /Users/xinference/Downloads/deploy88/loadtest/retest_direct.sh（第 3、5 行）
# 目标: 直连 32:8001 (不过HA)
TARGET='http://10.56.88.32:8001/v1/chat/completions'
```

**证据链**：脚本对应的落盘目录是 `Downloads/deploy88/loadtest/results/retest-single-viaHA-partial/`（目录名即 "via HA"）。它的 `B_1000_300s.log`、`B_2000_300s.log`、`B_5000_300s.log`、`B_10000_300s.log` 与 `1013/retest-single/` 的同名日志 **SHA256 逐一相同**（`9f4b3cad…` / `4228aba4…` / `18ced18d…` / `5cb6eded…`），只是 `all_progress.log` 里本仓副本缺了 B/20000 一行。⇒ **10:12 那批就是 `retest_single.sh` 跑出来的 `:8006` 经 HAProxy 批。**

## 3. 那一跳上到底改了什么（未钉死）

测试机上留下三份 HAProxy cfg，正好是三个候选状态，但**没有时间戳**能证明 10:12 当时在用哪份：

| 文件 | `defaults` 关键项 | 后端检查 |
|---|---|---|
| `haproxy.cfg.bak` | `mode http`、`option http-keep-alive`，**无** `http-reuse always` | `option httpchk GET /api/status` |
| `haproxy.cfg.l7` | `mode http`、`option http-keep-alive`、**`http-reuse always`**、`timeout http-keep-alive 10s` | `option httpchk GET /api/status` |
| `haproxy.cfg`（现留） | **`mode tcp`**、`option tcp-smart-accept/connect` | `option tcp-check` |

**一个合理的假设**（未验证）：HAProxy 在 `mode http` 下默认 `http-reuse safe`，对 POST 这类非安全请求默认不复用后端连接 ⇒ 每个请求新拨一条，首字就可能吃到建连 / delayed-ACK 那一档；`http-reuse always` 或 `mode tcp` 都能把它去掉。10:12 时首字已经回到 502.6ms，说明当时那一跳已经不是 `.bak` 那个状态（无论是切了 `http-reuse always`，还是切了 `mode tcp`）。

**这只是假设**：仓库只有三份 cfg 快照，没有做过「同一负载、只换一个 HAProxy 开关」的受控 A/B，因此**不能**把 205ms 精确归到某一个配置项上。要钉死，见 §6 的 A/B 步骤。

## 4. 对 AI 流式网关的含义：不是 HAProxy 不行，是默认配置不对路

AI 网关的流量形态与传统短请求 Web API 不同：请求是 **POST 且常常非幂等**，响应是 **长命 SSE / chunked 流**（单流 5.8s 起，长流 60s+），而且**首字节延迟（TTFT）比总时长更被用户感知**。通用 L7 反向代理的默认行为恰好在这几点上踩坑：

| 默认行为 | 对 AI 流式的后果 |
|---|---|
| `http-reuse safe`（POST 不保证复用后端连接） | 每请求新拨后端，首字吃建连/ACK 那一档 |
| 空闲 / keepalive 超时偏短 | 长流被中途切断或反复重连 |
| 健康检查打业务端点 | 探活变成额外负载，甚至掩盖真实失败 |
| 失败自动 retry | 把昂贵、非幂等的 LLM 请求重复计费/重复生成 |
| nginx 默认 `proxy_buffering on` | 逐 token 流被攒成整句/整段突发 |
| 模板里无条件写 `Connection: upgrade` | 悄悄掐掉 upstream keepalive，回到每请求新连 |

**HAProxy 本身能实时转发 SSE**（`mode tcp` 或 `mode http` 都可以，HAProxy **默认不缓冲响应体**，这点和 nginx 默认 `proxy_buffering on` 不同）。问题在于要把它显式配成流式友好的样子：

- **要极低延迟 / 纯透传** → `mode tcp`。最简单，代价是没有 HTTP 层健康检查与基于 header 的路由/重试。
- **要保留 HTTP 语义**（健康检查、header 改写、ACL 路由）→ `mode http` + `http-reuse always` + `timeout server/client`（覆盖 SSE 最长静默间隔）与 `timeout tunnel`（升级连接）+ 不配非幂等 POST 的自动 retry。这正是本仓给现场写的那套（[`../../deploy/v88/advanced/CONFIG.md`](../../deploy/v88/advanced/CONFIG.md)）。
- 换 nginx / Envoy / Traefik 不会自动更好：nginx 的 `proxy_buffering`、`Connection` 头、upstream keepalive 一样要手配。判据是「**不缓冲响应体、复用后端连接、超时覆盖最长流、不 retry 非幂等 POST、健康检查不碰业务重路径**」。

nginx 侧的一个实例（xgateway 仓库的 `deploy/nginx.conf`）：`proxy_buffering off` 已经写对，但那行 `proxy_set_header Connection 'upgrade';`（第 9 行）在非 WebSocket 场景会掐掉 upstream keepalive，建议改为：

```nginx
upstream xgateway_backend { server 127.0.0.1:3105; keepalive 64; }

location / {
    proxy_http_version 1.1;
    proxy_set_header Connection "";     # 不要无条件 upgrade
    proxy_buffering off;
    proxy_socket_keepalive on;
    gzip off;
}
```

（xgateway 上游客户端侧已在 `src/adapter/wire/client.rs:512`、`:628` 显式 `tcp_nodelay`；服务端 accept 侧同样应确认 NODELAY 且逐帧 flush。）

## 5. 如果把 HAProxy 按 AI 的方式重写：参考配置与取舍

> **先界定这不是「重写 HAProxy 源码」**：LB 该做的是「透明、低延迟地搬运长命流」；模型路由、token 计量/计费、prompt 改写、协议转换都应留在网关（xlite / xgateway），不该塞进 LB。下面是一份**面向 AI 流式**的参考 `haproxy.cfg` 与它背后的取舍。配置请用 `haproxy -c -f <cfg>` 解析通过后再上线（本仓无 HAProxy 二进制，未在本机解析过该示例）。

### 5.1 五条硬要求

1. **不缓冲响应体**：HAProxy 在 `mode http` 下默认就是边收边转（不像 nginx 默认 `proxy_buffering on`）；但**别**开 `option http-buffer-request` 这类会攒请求体的选项（它是为「必须重试」准备的，AI 场景我们不要重试）。
2. **复用后端连接**：`http-reuse always`。默认 `safe` 对 POST 不复用，每请求新拨一条，正是那 ~205ms 的形态。
3. **超时覆盖最长流**：SSE 是普通 HTTP 响应，管它的是 `timeout server`（后端静默上限）与 `timeout client`（客户端静默上限）；升级成 WebSocket 之类才用 `timeout tunnel`。三者都要 ≥ 最长可能的静默间隔（例如模型思考 60s），并用 `timeout client-fin`/`server-fin` 处理半关闭。
4. **不对非幂等 POST 自动重试**：`retries 0`（或只允许连接建立失败类），不要 `option redispatch`。一次 LLM 调用重试 = 重复生成、重复计费。
5. **负载均衡用 `leastconn`，不要 `roundrobin`**：AI 请求占连接的时间从 5s 到 60s+，roundrobin 会把新请求轮到一个正忙于长流的节点上；`leastconn` 才按在途连接数分流。

附加：健康检查打轻量端点（`GET /health`，别打 `/v1/chat/completions`）；不压缩（压缩要求攒完整个响应，等于消灭流式）；不做任何内容改写。

### 5.2 参考配置（L7 版，推荐）

```haproxy
global
    maxconn 500000
    nbthread 32
    tune.bufsize 32768          # 大 prompt / 大 header 需要
    tune.maxrewrite 8192
    tune.maxpollevents 50000
    stats socket /run/haproxy/admin.sock mode 660 level admin

defaults
    mode http
    option httplog
    option http-keep-alive
    http-reuse always           # 关键：POST 也复用后端连接
    retries 0                   # 关键：非幂等 POST 不自动重试
    timeout connect 5s
    timeout client 600s         # SSE：客户端静默上限
    timeout server 600s         # SSE：后端静默上限（覆盖模型思考间隔）
    timeout tunnel 3600s        # 升级连接（WebSocket 等）
    timeout client-fin 30s
    timeout server-fin 30s
    timeout http-request 30s
    timeout http-keep-alive 10s
    timeout queue 30s           # 后端排队超时，宁可快速 503 也不无限堆
    option tcp-smart-accept
    option tcp-smart-connect
    default-server check inter 2s fall 3 rise 2 maxconn 2000 slowstart 30s

frontend ai_in
    bind *:8006
    default_backend ai_gateways

backend ai_gateways
    balance leastconn           # 关键：按在途连接分流
    option httpchk GET /health
    http-check expect status 200
    server gw1 10.56.88.32:8001
    server gw2 10.56.88.33:8002
```

逐条对应 5.1：`http-reuse always`（复用）、`retries 0`（不重试）、`timeout server/client/tunnel`（覆盖最长流）、`balance leastconn`（长流分流）、`httpchk GET /health`（轻量探活）。

### 5.3 参考配置（L4 版，纯透传）

如果完全不想要 L7 语义，`mode tcp` 是最不容易引入首字延迟的形态：

```haproxy
global
    maxconn 500000
    nbthread 32
    stats socket /run/haproxy/admin.sock mode 660 level admin

defaults
    mode tcp
    option tcp-smart-accept
    option tcp-smart-connect
    timeout connect 5s
    timeout client 600s
    timeout server 600s
    timeout tunnel 3600s

frontend ai_in
    bind *:8006
    default_backend ai_gateways

backend ai_gateways
    balance leastconn
    option tcp-check
    server gw1 10.56.88.32:8001 check inter 2s fall 3 rise 2 maxconn 2000
    server gw2 10.56.88.33:8002 check inter 2s fall 3 rise 2 maxconn 2000
```

代价：没有 HTTP 健康检查（TCP 能连上就算健康）、没有 header 路由/改写。若网关本身有 `GET /health`，L7 版的探活更可信。

### 5.4 取舍表

| 决策 | AI 场景推荐 | 理由 |
|---|---|---|
| `mode http` vs `mode tcp` | 需 HTTP 探活/路由 → `http`；只要最低延迟与透传 → `tcp` | L7 语义换一点复杂度和风险 |
| `http-reuse` | `always` | 默认 `safe` 不保证复用 POST → 每请求新连，首字吃亏 |
| 均衡算法 | `leastconn` | 长流占连接，roundrobin 会把新请求堆到忙节点 |
| `retries` | `0` | 生成类请求非幂等，重试 = 重复生成/计费 |
| 请求体缓冲 | 关 | 攒请求体只会推迟首字 |
| 响应体缓冲 | HAProxy 默认关；确认别开 | 攒响应体 = 消灭流式 |
| 压缩 | 关 | 压缩需要完整缓冲 |
| 超时 | `server/client/tunnel/fin` ≥ 最长静默 | 别让 60s 长流被 idle 超时切断 |
| 健康检查 | 轻量 `GET /health` | 探活不该触发业务重路径 |

### 5.5 反模式

- `option http-buffer-request` / 任何「攒完整请求体再转发」；
- `option http-server-close` / `option forceclose`（每个响应后关后端连接，回到新连）；
- 默认 `retries 3` + `option redispatch`（非幂等重试）；
- 短 `timeout server`（会把长流切掉）；
- `balance roundrobin` 打长流；
- 健康检查打 `/v1/chat/completions`（探活变负载）；
- 试图在 LB 里做模型路由 / token 统计 / 内容改写（那是网关的事）；
- 无脑套用带 `Connection: upgrade` 或默认压缩的模板（nginx 常见）。

### 5.6 落地与验证

```sh
haproxy -c -f /etc/haproxy/haproxy.cfg          # 先解析
# 单流直连 vs 经 HAProxy，对比首字
curl -o /dev/null -w 'direct ttft=%{time_starttransfer}s\n' ... http://<gw>:8001/v1/chat/completions
curl -o /dev/null -w 'proxy  ttft=%{time_starttransfer}s\n' ... http://<haproxy>:8006/v1/chat/completions
# 观察后端连接是否复用（stats socket）
echo 'show stat' | socat stdio /run/haproxy/admin.sock | grep ai_gateways
```

判定：**经代理与直连的 `ttft` 差值应回到网关自身开销（几毫秒），`gw_oh` 不再出现 −205ms 那种固定负偏移**；同时 `dur`/`qps`/`chunk` 保持不变。

## 6. 压测时到底怎么打（对照纪律 + 验收）

**压测量「网关能力」**：k6 **直连**网关监听口（xlite `:8001`、xgateway `:8080`），谁都不挂 HAProxy。此时 `ttft ≈ mock 500ms + 网关转发开销（几毫秒）`，`gw_oh p50 ≈ 0` 反映真实转发。改法就是改 `TARGET_URL`：

```bash
# 经 HAProxy（会带上路径的 ~205ms）
TARGET=http://10.56.88.22:8005/v1/chat/completions
# 直连网关（去掉那一跳）
TARGET=http://10.56.88.24:8080/v1/chat/completions
```

`run_full_88_xg.sh` / `retest_*.sh` 都从 `TARGET_URL` 读目标，`env.txt` 里的 `TARGET=` 也要一起改，别只改命令行。

**压测量「客户现网」**：两侧都走**同一份、同一个（修好的）** HAProxy 配置。

**红线**：禁止「一侧直连、一侧过 Proxy」再拿差值写产品差距；禁止为了压 TTFT 提前回 200 / 先发空头 / 中间层攒响应体 / 短超时 / 降采样（见根 `AGENTS.md` 规则 9 与 [`../../deploy/v88/advanced/CONFIG.md`](../../deploy/v88/advanced/CONFIG.md) §2.5）。

**验收判据**（证明你修的是路径而非伪造首字）：

- 修好后 `gw_oh p50 → ~0`（不再是 −205ms）、`ttft p50 → ~502ms`；
- `dur p50`（5.87s）、`qps`（161）、`chunk p50`（11）**保持不变**；
- 直连单流先量一次基线：`curl -o /dev/null -w 'ttft=%{time_starttransfer}s\n' ... http://<gw>:<port>/v1/chat/completions`，应 ≈ 0.50s。若直连就已经是 0.70s，才轮到查网关自身的首块 flush。

**要钉死是哪个 HAProxy 开关**（把 §3 的假设变成结论），用同一负载、同一网关只换配置跑三档并比较 `gw_oh`：

1. `mode http`（无 `http-reuse always`）；
2. `mode http` + `http-reuse always`；
3. `mode tcp`。

## 7. 未决与边界

- **机制未钉死**：205ms 究竟由 `http-reuse safe`、`mode http` 的某处缓冲/时序，还是别的配置差异造成，尚无受控 A/B；§3 只是与三份 cfg 自洽的假设。
- **10:12 当时的具体 cfg 无时间戳**：只能证明「它经 HAProxy `:8006`」，不能证明当时那跳用的是哪一份 cfg。
- **发起脚本与日志副本不在本仓版本控制内**：`retest_single.sh` / `retest_direct.sh` 位于测试机与本地 `Downloads`，本仓只引用不复制；`retest-single-viaHA-partial/` 的 B 组四份日志与本仓 `1013/retest-single/` 逐字节一致。
- **`gw_oh` 是相对量**：以 mock 的自报时间戳跨度为基准，只在两者都产出 `ts` 时才有效；它可为负，单独一个 `gw_oh` 数值不能当「网关快慢」的裁定，必须与 `ttft`/`dur` 一起读。

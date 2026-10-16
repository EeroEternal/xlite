# mock-llm

实验室 Go `mock-llm:h2c` 上游 mock 的**同契约 Rust 复刻**：可审计的 drop-in 替代品。
原二进制（`mock-llm-linux-usage`，6.3 MB 静态 ELF、剥离符号、来源不明）无法审计也无法改；
本 crate 的契约由黑盒探测（strings + curl 实测，未反编译）还原，并用单元测试钉住。

## 定位

**价值在可审计与契约回归，不在绝对性能。** 实测与 Go mock 基本打平（见下文「性能」），
双方都要付逐帧计时器唤醒与 socket 写路径的固定成本，语言层面消不掉。

## 运行

```sh
cargo run --release -p mock-llm            # 默认 PORT=8081
PORT=4101 MOCK_TTFT=500 MOCK_BATCH=1 cargo run --release -p mock-llm
```

环境变量（与 Go 二进制同名同义）：

| 变量 | 默认 | 含义 |
|---|---|---|
| `PORT` | 8081 | 监听端口（0.0.0.0） |
| `MOCK_TTFT` | 500 | 首字前延迟 ms |
| `MOCK_OUT_TOKENS` | 50 | 每请求内容帧数 |
| `MOCK_TOKEN_INTERVAL` | 0 | 帧间隔 ms；**0 不是零延迟**，回落到 `MOCK_STREAM_DEFAULT_INTERVAL`（Go 怪癖） |
| `MOCK_STREAM_DEFAULT_INTERVAL` | 20 | 上一行的回落值 |
| `MOCK_BATCH` | 1 | 每 N 个内容帧合并一次 write（**不改载荷一个字节**） |
| `MOCK_TOTAL_DELAY` | 0 | 非流式额外延迟 ms（与 ttft 叠加） |
| `MOCK_MAX_DURATION` | 0 | 流式截断上限 ms（0 = 不截断） |
| `MOCK_RESPONSE_SIZE` | 0 | 内容总字节（0 = 4 字节/帧） |
| `MOCK_MIX` / `MOCK_LONG_PCT` / `MOCK_MID_PCT` / `MOCK_SHORT_PCT` / `MOCK_LONG_OUT` / `MOCK_MID_OUT` / `MOCK_SHORT_OUT` | 0 / 20 / 30 / 50 / 500 / 250 / 100 | `mix=1` 时按百分比抽 out_tokens 档位 |
| `MOCK_MODE` | 0 | 无可观测效果（Go 同），仅回读 |

## HTTP 契约

- `GET /health` → `{"status":"ok"}`
- `POST /_config` —— 无查询串=读；有=写。回读 JSON 与 Go 逐字节同形，**`batch` 是最后一个键**
  （压测 harness 用 `grep -o '"batch":[0-9]*}'` 取值，键序一变就 fail-closed）。
  未知键/非整数值静默跳过并在 stderr 留痕（Go 同款宽容语义）。
- `POST /v1/chat/completions` —— `"stream":true` 走 SSE（内容帧带 `ts` → finish 帧带 usage → `data: [DONE]`），
  否则一次性 JSON。单端口同时服务 HTTP/1.1 与 h2c（prior knowledge），
  h2c 恒定通告 `SETTINGS_MAX_CONCURRENT_STREAMS=250`（xlite 上游池据此定每连接流预算，必须同值）。
- 每请求覆盖头：`x-mock-ttft` / `x-mock-delay` / `x-mock-output` / `x-mock-size` / `x-mock-duration` / `x-mock-mode`；
  `x-mock-error: 500` → 500、`x-mock-error: rate_limit` → 429。

### 复刻的怪癖

- `token_interval=0` ⇒ 回落 `MOCK_STREAM_DEFAULT_INTERVAL`（默认 20ms），**不是**零延迟；
- `max_duration` 截断内容帧，但 `usage.completion_tokens` 仍报**配置值**；
- 非流式 body 以 `\n` 结尾；
- 内容填充词与 Go 的随机词库不同（帧大小、字段、时序一致，负载口径不受影响）。

## 性能（实测，不粉饰）

同负载（k6 2000 VU、500 chunk/请求、本机 22 核）：

| 档位 | Go mock | mock-llm |
|---|---|---|
| `batch=1`，interval=10ms | 26 CPU-s | 26 CPU-s（打平） |
| `batch=5`，interval=10ms | 12 CPU-s | 22 CPU-s（Go 赢） |
| 非流式 6.5 万 req/s | 0.14 核 | 同量级 |

残差在 hyper 写路径与逐帧计时器唤醒——框架固定成本，双方都要付。已做零分配帧拼装
（常量切片 + 13 位 `ts` 就地修补、每流一次 `BytesMut`）与计时器复用（`Sleep::reset`
而非逐帧新建），再往下抠收益递减。**mock 本就不是压测栈的瓶颈**。

## 测试

```sh
cargo test -p mock-llm
```

7 个单元测试钉住契约：`_config` 回读形态（batch 末位）、流帧形状与 `ts`、batch 不改载荷、
`max_duration` 截断语义、错误注入哨兵、h2c `SETTINGS=250`、非流式 JSON 形状。

## 边界

- `x-mock-tps` / `x-mock-mode` 探测无可观测效果，未实现行为（mode 仅回读）；
- 未做 24 口农场吞吐混跑对拍；
- 内容填充为确定性短语（Go 侧为随机词库）。

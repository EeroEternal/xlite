# deploy/v88 —— 测试环境启动

口径见 [`../../docs/manual/v88-rig.md`](../../docs/manual/v88-rig.md)，各配置项取值见 [`advanced/CONFIG.md`](advanced/CONFIG.md)。

```
k6 ──HTTP/1.1──► HAProxy :8005/:8006 ──► xlite ──h2c──► mock-llm :8081-8104
                                            └─ Redis：目录快照（请求路径不碰）
```

| 机器 | 角色 | 端口 |
| --- | --- | --- |
| `10.56.88.22` | Redis（现场那台还有 PG / HAProxy） | 6379 / 8005·8006 |
| `10.56.88.24`、`10.56.88.26` | 网关 xlite（两台各一份） | 8080 |
| `10.56.88.29` | mock 农场（24 个）+ 压测 | 8081-8104 |

## 1. mock 农场（8081-8104）

```sh
docker load -i <mock 包>/mock-llm-h2c.tar.gz          # 客户包里的 mock-llm-h2c.tar.gz

mkdir -p /root/loadtest/mock
cp -f <mock 包>/mock-llm-linux-usage /root/loadtest/mock/
chmod +x /root/loadtest/mock/mock-llm-linux-usage
for p in $(seq 8081 8104); do
  docker rm -f mock51h-$p 2>/dev/null || true
done
for p in $(seq 8081 8104); do
  docker run -d --name mock51h-$p --network host --restart unless-stopped \
    --ulimit nofile=1048576:1048576 \
    -v /root/loadtest/mock/mock-llm-linux-usage:/mock-llm:ro \
    --entrypoint /mock-llm \
    -e PORT=$p \
    mock-llm:h2c >/dev/null
done
sleep 2
OK=0
for p in $(seq 8081 8104); do
  curl -s -m 3 http://127.0.0.1:$p/health | grep -q ok && OK=$((OK+1))
done
echo "mock 健康: $OK/24"
```

停掉/清理（换镜像或重跑前）：

```sh
for p in $(seq 8081 8104); do docker rm -f mock51h-$p; done
```

下发口径 + 自检（单流应得 502 行 `data:` / ≈100 736 B / 500 个 `ts`）：

```sh
for p in $(seq 8081 8104); do curl -s -m 3 -X POST \
  "http://127.0.0.1:$p/_config?ttft=500&token_interval=10&out_tokens=500&mix=0&mode=2&batch=1" >/dev/null; done
```

## 2. Redis

用现场 22 上那台（与 New API 共用）：**不要新起、也不要改它的启动参数**，只把网关的 `REDIS_URL` 指过去。xlite 在里面只占 `xlite:catalog`（约 7KB）+ 写入短锁，互不影响；目录数据本来就在里面，不用再导入。

> 现场另有一套**外接哨兵 Redis 集群**（`redis+sentinel://…`）。**当前 xlite 不支持这个 URL** —— 实测报 `parse REDIS_URL` / `Redis URL did not parse - InvalidClientConfig`（连接走 `redis::Client::open`，代码里没有 sentinel）。要用它得先给 xlite 加 sentinel 支持；在那之前只能指单副本那台。

## 3. 网关

```sh
docker run -d --name xlite-gw --network host --ulimit nofile=1048576:1048576 \
  -e TOKIO_WORKER_THREADS=8 -e REDIS_URL=redis://10.56.88.22:6379 \
  -e XLITE_UPSTREAM_H2C=1 -e XLITE_UPSTREAM_MAX_CONNS_PER_HOST=32 \
  -e ADMIN_PASSWORD='<自己定>' -e LISTEN_HOST=0.0.0.0 -e PORT=8080 \
  xlite:0.5.0
```

## 4. HAProxy（两个 xlite）

```haproxy
global
    maxconn 200000
    log stdout format raw local0

defaults
    mode http
    log global
    option http-keep-alive
    timeout connect 10s
    timeout client 600s
    timeout server 600s
    timeout tunnel 600s
    timeout http-request 60s

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
    server n24 10.56.88.24:8080 check inter 5s fall 3 rise 2
    server n26 10.56.88.26:8080 check inter 5s fall 3 rise 2

backend xgw_single
    balance roundrobin
    option httpchk GET /health
    http-check expect status 200
    server n24 10.56.88.24:8080 check inter 5s fall 3 rise 2
```

`:8005` 打两台（`n24`+`n26`），`:8006` 只打 `n24`（单节点对照口）；两个 `server` 就是两个 xlite 的 `IP:PORT`（客户现场 `PORT=8080`）。

## 5. 跑

```sh
cd deploy/v88
export TARGET=http://<入口>/v1/chat/completions TOKEN=<key>
./run_full_88_xg.sh
```

入口填 `:8005`（双节点）或 `:8006`（单节点）；`TOKEN` 会变——在网关网页重置密钥后复制过来（命令行导入的目录默认是 `sk-loadtest`）。

### 5.1 阶梯形状与墙钟（决定要不要分轮跑）

默认是**两段步进**：低载段 1000 一档（对标最干净的那张表——1000~4000 VU 的 TTFT 就在这一段），
VU ≥ `STEP_COARSE_ABOVE`（默认 10000）之后放粗到 `STEP_COARSE`（默认 5000）：

```
1000 2000 3000 4000 5000 6000 7000 8000 9000 10000 15000 20000 … 55000 60000
```

⇒ 每组 **20 档**、六组共 **120 档**。末档**一定**是 60000——步进粗过 `MAX_VUS` 时 driver 会
补一档（例：均匀 `STEP=5000` 时算式只到 56000）。因为：

> **`MAX_VUS` 不得低于 60000**（客户双节点那轮的上界）：低一档对标表就缺行，driver 直接
> `exit 2` 并提示该调什么。**要缩一场跑测请调步进/稳态/分轮，不要降上界。**

开跑前 driver 会把档数与墙钟算给你（同时进 `progress.log` 与 `env.txt`）：

```
[HH:MM:SS] 阶梯计划：组 B D E F A C 各 20 档 ⇒ 共 120 档；载荷窗口合计 ≈ 16.7 小时
[HH:MM:SS] ⚠️ 口径变更：VU ≥ 10000 起用步进 5000（比客户阶梯稀疏，已记入 env.txt）
```

真实墙钟主要在**排空**上：实测每档约 **9 分钟**（0927 轮 78 档 / 11.8 小时），高 VU 档可到 12 分以上。
所以 120 档 ≈ **18 小时**——**不要指望它不被打断**：0927 那轮 `progress.log` 里 `组完成` 标记为 **0**，
E 组 37000 被外部中断，F/A/C 从未开始。

**四个旋钮（都不动上界）：**

| 想要 | 怎么设 | 每组档数 |
| --- | --- | ---: |
| 与客户**逐档一致**的密阶梯 | `STEP_COARSE_ABOVE=0` | 60 |
| 更短（≈ 11 小时） | `STEP_COARSE_ABOVE=5000 STEP_COARSE=5000` | 16 |
| 完全均匀粗步进 | `STEP=5000 STEP_COARSE_ABOVE=0`（末档自动补到 60000） | 13 |
| 缩短稳态（也是口径变更） | `STABLE_SHORT=120s` | — |

**两个“看不见的洞”（driver 开跑前会检查并把实际值记进 `env.txt`）：**

- **`GRACEFUL_STOP` 必须比 `STREAM_TIMEOUT` 留余量** —— 前者是 Go 时长字面量（`900s`、`10m0s`），
  后者是毫秒（`600000`）。余量不足时，在载荷窗口末尾发起的请求会在“被记成失败”之前就被 k6 掐掉，
  `fail%` 与 p99 被**低估**，而且日志上看不出来（0927/0924 两轮的 `interrupted iterations` 都是 0，
  这条余量从未被触发过）。默认 `900s` 对 `600000ms` 余 **300s**，不改就不用管；实际余量记在
  `env.txt` 的 `GRACE_MARGIN_S`。⚠️ **客户那份口径是 `600s` / `600000ms`，余量为 0** —— 要 1:1
  复刻它就必须接受这个洞，所以 driver 只告警、不拦。
- **`K6_MEM` 是 docker 的 `-m`：上限，不是预留。** 它同时是本机 k6 的护盾 —— 设小了 k6 自己被杀
  （`rc=137`；0927 那轮 5 档如此，被迫降到 `KEEP_SAMPLE=0.01` 重跑，**那 5 档的口径就与全表不同**）；
  设大了护盾消失，宿主机 OOM killer 挑受害者，而现场 `.29` 上**同时住着 24 个 mock**，被杀的可能不是 k6。
  driver 会读 `/proc/meminfo`，`K6_MEM` 超过本机内存一半就告警，并把 `K6_MEM` / `HOST_MEM_TOTAL_G`
  记进 `env.txt`。**升 `K6_MEM` 之前先 `free -g`**；余量薄时优先“把 k6 挪出 mock 机”，而不是抬上限。

**分轮续跑**（推荐，把 ≈18 小时切成几个窗口）：完整开跑流程、每轮要核对的机械判据、以及三个容易踩的点，见 §6。

粗步进为什么安全：高 VU 段（实测 B 组 44000 之后、D 组 16000 之后）本来就没有干净数据，
把它密到 1000 步进的收益很低，而每档都要付一份排空时间。

## 6. 下次现场怎么开（建议）

目标：**上界保持 60000，任何一轮被打断都能续**。默认两段步进（§5.1）把每组压到 20 档，
按每档 ~9 分钟 ≈ **3 小时/组**，六组分成六个时间窗。

```sh
cd deploy/v88                                    # 现场是整目录拷过来的
export TARGET=http://10.56.88.22:8005/v1/chat/completions
export TOKEN=<在网关网页重置后复制>
export RESULT_DIR=/root/v88-run1                 # ★ 所有轮次共用，续跑全靠它

# 只做前置检查（不起 k6）：封条 / k6 镜像 / 农场可达 / 路数一致
./run_full_88_xg.sh --preflight-only

# 1) 第 1 轮
./run_full_88_xg.sh --combo B

# 2~6) 每轮一个组，**同一个 RESULT_DIR** + RESUME=1
RESUME=1 ./run_full_88_xg.sh --combo D
RESUME=1 ./run_full_88_xg.sh --combo E
RESUME=1 ./run_full_88_xg.sh --combo F
RESUME=1 ./run_full_88_xg.sh --combo A
RESUME=1 ./run_full_88_xg.sh --combo C
```

阶梯形状与停测规则**取脚本默认**（`STEP_COARSE_ABOVE=10000` / `STEP_COARSE=5000` /
`STOP_ON_FAIL=0`，含义见 §5.1），所以上面没有 export 它们。**要改就必须 `export`**：
写成前缀形式（`STEP_COARSE_ABOVE=20000 ./run_full_88_xg.sh --combo B`）**只对那一条命令生效，
下一轮会静默退回默认**——同一份数据就混了两种口径（实测：前缀形式第 1 轮 28 档、第 2 轮 20 档）。
实际生效值一律以结果目录的 `env.txt` 为准（它记 `STEP_COARSE_ABOVE` / `STEP_COARSE` /
`STABLE_SHORT` / `STABLE_LONG` / `RESUME` / `STOP_ON_FAIL` / `LADDER_LEVELS_*`）。

把 `STEP_COARSE_ABOVE` 调到 `20000` 会把 10000–20000 也留成密档，每组 28 档；想更短就往下调
（`=5000` ⇒ 16 档）。**但不要动 `MAX_VUS`**。

**每轮开跑前先核对这三行**（`progress.log` 开头）：

```
[HH:MM:SS] 阶梯计划：组 B D E F A C 各 20 档 ⇒ 共 120 档；载荷窗口合计 ≈ 16.7 小时（真实墙钟更长…）
[HH:MM:SS] ⚠️ 口径变更：VU ≥ 10000 起用步进 5000（比客户阶梯稀疏，已记入 env.txt）
[HH:MM:SS] ♻ RESUME=1：已有 rc=0 的档将复用旧日志、不再跑 k6
```

**每轮结束后查三件事**（都是机械判据）：

| 查什么 | 命令（在结果目录里） | 期望 |
| --- | --- | --- |
| 新列真有数 | `grep -c 'split(t=? 502=?' progress.log` | `0`；有数时长这样 `split(t=… 502=… 503=… other=…)` |
| 有没有被截断 | `grep -c '组完成' progress.log` | = 已完成组数。**0927 那轮这里是 0**，才发现整轮被外部中断 |
| 复用只发生在旧档上 | `grep '♻' progress.log` | 只出现在之前轮次已经跑完的档 |

> ⚠️ 这三条**只对用本版 driver 跑的 `progress.log` 成立**。旧轮次的日志里根本没有 `split(...)`
> 这一列（那时还没加），拿它跑 `grep -c 'split(t=? 502=?'` 会得到 `0` 而看似通过——
> 先确认日志里有这个列，再拿它做判据。

**三个容易踩的点：**

- **`RESUME=1` 复用的是旧轮次的日志**，`env.txt` 里最后那份 `RESUME=1` 不代表“这批数据是新跑的”。
  判读看每档日志自己的时间戳；每档还带一个 `.rc` 文件（只有 `0` 才被复用）。
- 每轮会**重下发农场配置并打一发冒烟**（`<组>_smoke.log` / `.body`）。冒烟失败只告警、不中止。
- `--combo` 一次只收一个组；只想抽查几档用 `--levels 1000,5000,10000`。

## 7. 核对清单

| # | 机器 | 做什么 | 怎么验证 |
| --- | --- | --- | --- |
| 1 | 22 | Redis 在跑（别人的，**别动**） | `redis-cli -h 10.56.88.22 ping` → `PONG`（或在 22 上 `docker exec redis redis-cli ping`） |
| 2 | 22 | HAProxy 在跑（cfg 见 §4） | `curl -s http://10.56.88.22:8005/health` → `{"status":"ok",…}` |
| 3 | 24 / 26 | 起 xlite（§3） | `curl -s http://10.56.88.24:8080/health`、`…/ready` |
| 4 | 29 | 起 24 个 mock（§1） | 脚本打印 `mock 健康: 24/24` |
| 5 | 29 | 下发口径 + 单流自检（§1） | 502 行 `data:` / ≈100 736 B / 500 个 `ts` |
| 6 | 29 | 前置检查 | `TARGET=… TOKEN=… ./run_full_88_xg.sh --preflight-only` → `RC=0` |
| 7 | 29 | 开跑（分轮跑见 §6） | `./run_full_88_xg.sh`；结果目录里看 `progress.log`（逐档一行）与 `env.txt`（口径凭据） |

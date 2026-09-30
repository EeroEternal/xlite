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
  xlite:0.5.2
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
    server n24 10.56.88.24:8080 check inter 5s fall 5 rise 2
    server n26 10.56.88.26:8080 check inter 5s fall 5 rise 2

backend xgw_single
    balance roundrobin
    option httpchk GET /health
    http-check expect status 200
    server n24 10.56.88.24:8080 check inter 5s fall 5 rise 2
```

`:8005` 打两台（`n24`+`n26`），`:8006` 只打 `n24`（单节点对照口）；两个 `server` 就是两个 xlite 的 `IP:PORT`（客户现场 `PORT=8080`）。

**`maxconn 200000` 要它的 fd 上限配合**：`maxconn` 只是 HAProxy 的**内部计数**，真正的 fd 上限由启动它的环境给（systemd / 容器 / 父 shell），HAProxy 会自己按 `maxconn` 去抬、但**抬不过 hard 上限**。抬不上去时它不报错，客户端只看到 **RST** 和 **`dial: i/o timeout`**。上 `.22` 对一次账：

```sh
pid=$(pgrep -f haproxy | head -1)
grep -i 'open files' /proc/$pid/limits                                    # soft 要 ≥ 210000
journalctl -u haproxy -b --no-pager | grep -iE 'ulimit|open files' | tail -3
```

不够就按 `advanced/CONFIG.md` §3.2 改（**必须 `restart`，`reload` 不重读 `LimitNOFILE`**）；链路上其余五处上限（网关 / k6 / conntrack / somaxconn / 出站端口）见同文件 §3.1。

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

**三个“看不见的洞”（前两条 driver 开跑前会检查并把实际值记进 `env.txt`，第三条要手工查一次）：**

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

    **对账公式：`K6_MEM` + 24 个 mock + OS ≤ 宿主总量。** 现场 `.29` 实测（`free -g`）是
    **`Mem: total 61`**（**不是 128G**）+ **swap 15G** —— 这一条直接否掉默认值：

    > ⚠️ **61G 宿主上不要用 `K6_MEM=80g`。** `docker -m` 是**上限不是预留**：宿主只有 61G，
    > 80G 的天花板**永远够不到**，等于 k6 **没有内存限制**；它一旦长起来撞的是宿主 OOM killer，
    > 而**被杀的可能换成 mock**。0927 那轮的反面证据：当时默认就是 `K6_MEM=45g`，k6 在
    > 45000/49000/52000/54000/55000 被杀了 **5 次**（`rc=137`），而 **24 个 mock 全部活下来** ——
    > 护盾正常工作，代价是那 5 档走了降采样重试（已记入 `DOWNGRADED_LEVELS`）。
    > ⇒ **在 `.29` 上显式 `export K6_MEM=45g`**（有实测背书），别用默认的 80g。

    开跑前（就在小跑那几档时）量一下 mock 的真实占用：

    ```sh
    # A) 24 个 mock 的 RSS 合计（宿主机 ps 能看到容器进程）
    ps -eo rss,comm | awk '/mock-llm/ {s+=$1} END {printf "mocks RSS 合计 %.1f GiB (n=%d)\n", s/1048576, NR}'
    # B) 逐容器（更准，含 Go runtime 开销）
    docker stats --no-stream --format '{{.Name}} {{.MemUsage}}' | grep mock51h
    ```

    mocks 合计 ≪ 16G（比如 <8G）⇒ 45g 还有余量，可试到 50g；逼近或超过 16G ⇒ **把 k6 压低**，
    并接受高档位更容易被杀。

    ⚠️ 在 61G 宿主上**只要 `K6_MEM` > 30g 就会告警**（阈值是宿主的一半）：

    | `K6_MEM` | 61G 宿主 | 读法 |
    | ---: | --- | --- |
    | 80g | ⚠️ | 天花板高于宿主 ⇒ **等于无限制**，别用 |
    | 45g | ⚠️ | 阈值保守；0927 实测这台机上 45g + 24 mocks 能跑通 ⇒ **可接受** |
    | 30g | ok | 只有它不告警，但很可能不够 k6 用 |

    所以**告警可以接受，但别拿它当把值往 80g 抬的借口**。

  - **`swap` 要单独查一次：driver 没有设 `--memory-swap`。** `docker run … -m "$K6_MEM"` 只给了内存上限、
    **没给 swap 上限**，而 `.29` 有 **15G swap**。若当前 Docker/cgroup 允许这个容器用 swap，k6 内存吃紧时
    会**换出到磁盘** —— 整机 IO 抖动、延迟数据失真，而且**比被杀更糟：日志上看不出来**。
    小跑进行中核一次（容器名形如 `v88_k6_<driver pid>`）：

    ```sh
    docker ps --format '{{.Names}}' | grep v88_k6          # 拿名字
    docker inspect <名字> --format '{{.HostConfig.Memory}} / {{.HostConfig.MemorySwap}}'
    ```

    - `MemorySwap` = 0 或 = 2 × `Memory` ⇒ 容器可用 swap，建议给 driver 加上
      `--memory-swap="$K6_MEM"`（等价于该容器**禁用 swap**）；
    - 已经等于 `Memory` ⇒ 没问题。

    （Docker 在只给 `-m` 时对 swap 的默认语义随版本与 cgroup 版本而变 —— **不要假定，查一次**。）

**分轮续跑**（推荐，把 ≈18 小时切成几个窗口）：完整开跑流程、每轮要核对的机械判据、以及三个容易踩的点，见 §6。

粗步进为什么安全：高 VU 段（实测 B 组 44000 之后、D 组 16000 之后）本来就没有干净数据，
把它密到 1000 步进的收益很低，而每档都要付一份排空时间。

## 6. 下次现场怎么开（建议）

目标：**上界保持 60000；任何一组被打断，重跑同一组就能从断点接上**。默认两段步进（§5.1）把每组压到 20 档，按每档 ~9 分钟 ≈ **3 小时/组**，六组分成六个时间窗。

```sh
cd deploy/v88                                    # 现场是整目录拷过来的
export TARGET=http://10.56.88.22:8005/v1/chat/completions
export TOKEN=<在网关网页重置后复制>
export RESULT_DIR=/root/v88-run1                 # ★ 一张阶梯一个目录，六组都落这里

# 只做前置检查（不起 k6）：封条 / k6 镜像 / 农场可达 / 路数一致
./run_full_88_xg.sh --preflight-only

# 一张阶梯 = 六轮，每轮一个组。**每组只跑一次时不需要 RESUME**
./run_full_88_xg.sh --combo B
./run_full_88_xg.sh --combo D
./run_full_88_xg.sh --combo E
./run_full_88_xg.sh --combo F
./run_full_88_xg.sh --combo A
./run_full_88_xg.sh --combo C

# 某一组被打断（或想补齐漏档）→ 加 RESUME=1 **重跑那一组**：组内已完成且 rc=0 的档
# 直接复用旧日志、不再跑 k6，从断点继续。例：B 组跑到 23000 被打断：
RESUME=1 ./run_full_88_xg.sh --combo B
```

**`RESUME` 管的是「组内」，不是「跨组」** —— 这一点很容易读反：

| 想做的事 | 靠什么 | 需要 `RESUME=1` 吗 |
| --- | --- | --- |
| 接着跑**另一个组** | `--combo <组>`（`run_group` 只枚举那一组的档） | **不需要**：`--combo D` 根本不会去看 `B_*.log`（复用判据的 key 是 `${combo}_${vus}`；实测：同一目录里只有 B 的结果时，`--combo B` 复用、`--combo D` 重跑） |
| 接着跑**同一个组**剩下的档 | `RESUME=1` + 同一个 `RESULT_DIR` | **需要**：否则从 `START_VUS` 整组重来 |

`RESULT_DIR` 的作用是「一张阶梯的所有结果放一处 + 让 `RESUME` 有地方找旧日志」，它本身不会替你记进度。

阶梯形状与停测规则**取脚本默认**（`STEP_COARSE_ABOVE=10000` / `STEP_COARSE=5000` /
`STOP_ON_FAIL=0`，含义见 §5.1），所以上面没有 export 它们。**要改就必须 `export`**：
写成前缀形式（`STEP_COARSE_ABOVE=20000 ./run_full_88_xg.sh --combo B`）**只对那一条命令生效，
下一轮会静默退回默认**——同一份数据就混了两种口径（实测：前缀形式第 1 轮 28 档、第 2 轮 20 档）。

⚠️ **`env.txt` 是每轮覆盖写的**，所以多轮跑完之后它只反映**最后一轮**的设置（只有
`DOWNGRADED_LEVELS=` 与 `LADDER_LEVELS_*` 是追加的）。跨轮核对口径要看每轮 `progress.log`
开头那三行——这正是「跨轮别改口径」这条要求得人自己守的原因。

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
- `--combo` 一次只收一个组；只想抽查几档用 `--levels 1000,5000,10000`。**它是阶梯的过滤器**：值必须正好落在当前阶梯上，否则 driver 直接 `exit 2` 并**打印当前阶梯**供复制（档位值很容易从**旧结果的文件名**加 `B_11000.log` 抄来，而旧轮次的阶梯与当前未必一样——默认阶梯是 1000 起 +1000、且 VU ≥ 10000 改 5000 步进，所以 11000 / 31000 / 44000 都不在上面）。过滤后的实际集合会打进 `progress.log` 与 `env.txt`（`EFFECTIVE_COMBOS` / `EFFECTIVE_LEVELS_PER_GROUP`）。

### 6.1 开跑前先量一次池尺寸（`STREAM_CAP` / `MAX_CONNS_PER_HOST`）

**为什么必须先量**：`XLITE_UPSTREAM_STREAM_CAP` **只在真的绑定**时才有代价。实验室两轮实测（24 口农场 / 16000 VU / `MAX_CONNS=25`，每档都过「`POOL_ERR=0` + 单流预检 200」闸门）：CAP=30 的 `ttft p50` 比"不设"高 **+223~232ms**，换来的是尾部（`dur p90` −2s）与吞吐（+8%）；**没绑定时改它一个数都不会变**。所以"该设几"要在**这台机上**量出来，不能照搬实验室值。

**怎么做**（`.24` / `.26` **各一份**，取更紧的那次）：

```sh
# ① 用**现状配置**先跑一轮目标档（例如 B/45000）；跑到量之后，在**同一台网关机上**执行：
./advanced/pool_sizing_probe.sh --container xlite-gateway --gw-port 8080 --seconds 120

# 0.5.2 起更推荐：给了管理员密码就**优先读 /api/pool**（精确到每条 host 的连接数与在途
# 流数，不需要 ss / docker；读不到会自动回落到上面那条 ss 路径，并在报告里注明）
./advanced/pool_sizing_probe.sh --gw-url http://127.0.0.1:8080 --admin-password "$ADMIN_PASSWORD" --seconds 120
#    只读：不改系统、不重启容器、不写配置。

# ② 看「判定」那一行：
#      binding / partial ⇒ CAP 不设（跟随对端）+ MAX_CONNS 取探针给的值
#      not-binding       ⇒ **别动 CAP**：首字问题不在这条轴上，回来查别的原因

# ③ 改完 → 空跑一轮 warmup → 再跑同一档；报告里贴探针最后那段「口径变更」说明。
#    ⚠️ 两台必须一致（HAProxy 是 dual，配置不同 = 两条延迟曲线被合在一起）；
#    ⚠️ `docker restart` 不改 env，必须按原命令 / compose 重建容器。
```

**今天的起点**：`.24` / `.26` 是 `STREAM_CAP=30` + `MAX_CONNS=25`（= 每通道 **750** 流的上界）。若探针判 `binding` / `partial`，标准换法是：

```
XLITE_UPSTREAM_STREAM_CAP                         不设（跟随对端，常为 250）
XLITE_UPSTREAM_MAX_CONNS_PER_HOST                 3        # 3 × 250 = 750，上界与今天完全一致
```

**这不是"两全"**：`ttft p50` 预计 **−190~220ms**，代价是 `dur p90` **+~2s**、`qps` **−4~8%**。完整曲线、以及"125 看似折中但有 5% 余量陷阱"的实测见 [`advanced/CONFIG.md`](advanced/CONFIG.md) §5.1。

**探针的边界**（照抄进报告不会被问住）：

| 项 | 说明 |
| --- | --- |
| 退出码 | `0` 判定有效 / `2` 负载太低不可信 / `3` 缺工具 / **`4` 每口采样不可用 ⇒ 拒绝给结论** |
| 在哪跑 | 回落路径（`ss`）必须在**网关机**上：它按"本机到 `8081-8104` 的出站连接"统计；网关与农场同机会把别的进程算进来。**给了 `--admin-password` 走 `/api/pool` 时没有这个限制** |
| 「在途」口径 | `/api/pool` 模式下是**精确读数**；回落模式下默认用**入站连接数**（上界 —— VU 在两次请求之间也占着连接），给 `--k6-report <k6日志>` 会用 `iterations/s × dur p50` 校准，`--inflight N` 可直接指定 |
| 自检 | `./advanced/pool_sizing_probe.sh --self-test` |

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
| 8 | 22 / 24 / 26 / 29 | 链路上的 fd / 表项上限一次查完（`advanced/CONFIG.md` §3.1） | 逐项对照期望值；**只有低于期望的才改** |
| 9 | 24 / 26 | **先量再部署**：定 `STREAM_CAP` / `MAX_CONNS`（§6.1） | `./advanced/pool_sizing_probe.sh` 退出码 `0` 且「判定」行有值；两台配置一致 |

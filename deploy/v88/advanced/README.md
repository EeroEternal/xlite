# deploy/v88/advanced —— 框架说明与专用工具

> **部署/跑测只看 [`../STARTUP.md`](../STARTUP.md)**；本目录是细节与专用工具，部署人员不需要读。

一套**能带走、能复现、口径不会被悄悄改掉**的压测框架。客户包（`run_full_88.sh` + 两份
`k6.js`）在 `bench/` 下且被 `.gitignore` 忽略，**不随 clone 到现场**；本包是它的
可版本控制、可机械校验的替代。

口径契约与判读纪律以 [`docs/manual/v88-rig.md`](../../../docs/manual/v88-rig.md) 为准，
本文只讲**框架本身**。

---

## 1. 为什么要另起一套

| 客户原版的问题 | 本框架的做法 |
| --- | --- |
| 四个 driver 都 `-v /root/loadtest/k6:/k6`，**只认路径、不校验内容** —— 对标那晚同一路径下的文件被手改过至少四次，qps 从 1033 跳到 1912、TTFT 从 714ms 涨到 1747ms，日志上看不出来 | **preflight 校验哈希**：副本必须逐字节等于规范源，否则 exit 2 |
| `run_full_88.sh:68` 把 `batch=1` 写死，且 `curl … >/dev/null 2>&1` **不检查结果** | `MOCK_BATCH` 参数化 + 逐口**回读校验** + 收尾还原 + 每轮记账 |
| `deploy88/start-mocks.sh` 起 **12** 口，driver 却按 **24** 口配（`seq 8081 8104`），配不上的口完全静默 | 农场路数显式校验：读不到口径的口直接判失败并中止 |
| 长流组（D `out=6000`）每次迭代给 `chunk_interval_ms` 塞 **5999** 个 Trend 样本 —— k6 高 VU 下 OOM 的主因 | `CHUNK_MAX` 有界采样（默认 64/次迭代）+ 上报 `chunk_samples_added`；k6 崩溃时降采样重试并**记为口径变更** |
| 一场跑满要几十小时，中途被打断就是一份带缺口的数据（0927 轮 78 档用 11.8 小时，`组完成` 标记为 **0**） | 开跑前先把**档数与载荷窗口**算出来打进 `progress.log` 与 `env.txt`；`MAX_VUS` **不得低于 v88-dual 的 60000**（低于就 `exit 2`）；要缩墙钟用 `STEP_COARSE_ABOVE`/`STEP_COARSE`、稳态，或 `RESUME=1` 分轮跑（复用的档标 `♻`）。阶梯形状（含粗步进这种口径变更）全部记入 `env.txt` |
| 一路顶到连续两档 fail>1% 才停（过载段被截断，事后无法解释） | **默认不停**：跑到 `MAX_VUS`，过载档只标 ⚠️ 不截断（`STOP_ON_FAIL=1` 可恢复客户原版规则）。内置 `fail%` 口径不变；另外把「传输错误 / 502 / 503 / 其它非 200」拆成独立 Counter，每档摘要多打 `reqs=` / `split(t= 502= 503= other=)` / `rst=` / `dial=`。找天花板用 `farm_ceiling_scan.sh` |

## 2. 目录

**上一级 `deploy/v88/`（部署用，见 [`../STARTUP.md`](../STARTUP.md)）**

| 文件 | 是什么 |
| --- | --- |
| `../STARTUP.md` | 启动手册：mock 农场 / Redis / 网关 / HAProxy / 跑 |
| `../run_full_88_xg.sh` | 客户 `run_full_88.sh` 的 xg 版（主驱动器） |
| `../k6.js` | 喂负载脚本（现场挂给 k6 的 `/k6/k6.js`）；改它要**重新打封条** |
| `../lib/lib_v88.sh` / `../lib/lib_rig.sh` | 共用件：lint / 封条自检 / 农场控制 / 产能预算 / 台架互斥锁；改 `lib_rig.sh` 要**重新打封条**（`lib_v88.sh` 不在封条里） |
| `../lib/selftest_farm_hosts.sh` | 自证 `MOCKS` 的 5 种写法（本机/跨机 × 区间/列表）展开正确（11 项断言）；**手动门禁**，preflight 不跑它 |
| `../seal_bundle.sh` / `../BUNDLE_SHA256` | 自封条工具与封条本身（preflight 每次校验） |

**本目录 `advanced/`（按需读）**

| 文件 | 是什么 | 能不能改 |
| --- | --- | --- |
| `CONFIG.md` | **现场配置文档**：`TOKIO_WORKER_THREADS` 怎么设、HAProxy 怎么配、内核/fd、不要设什么、上线核对清单 | 框架自有（数值以仓库实测为准） |
| `farm_ceiling_scan.sh` | 在**你们的**机器上实测农场真实上限（决定加几台 mock） | 框架自有 |
| `ab_topology.sh` | 瓶颈归因：网关 / mock 农场 / k6 各吃多少核 | 改它要**重新打封条** |
| `sample_three_walls.sh` | **只读**采样器：压测**进行中**把三面墙（农场每口 / k6 / 网关）+ accept 队列 + `/health` 响应时间同时采下来；**四台机各跑一份、按行首时间戳对齐**。不 ssh、不取台架锁、不改任何配置 | 框架自有（**不在封条里**，改了不用重打） |
| `v88.env.example` | 全部可调口径的变量清单（**脚本不读文件**，直接 export 或 `. ` 进 shell） | 你填 |

### 这个包是**自足包**：不依赖仓库的任何路径

`deploy/v88/` 可以**整目录**拷到客户机（离线、无 git、无仓库）直接跑。所以：

- **不与 `scripts/loadtest/` 做任何比对** —— 没有 `sync_canonical.sh`，没有跨目录哈希校验。
- 代价是**两边可能漂移**，靠**流程**而不是机械门禁 —— 这是明确的取舍。
- 但仍然保留一条**机械**检查：`BUNDLE_SHA256` 自封条。它抓的恰恰是 §1.1 那个**真实故障形态** ——
  文件被拷到现场机上之后**被人就地改掉**（那晚 `/root/loadtest/k6/k6.js` 被手改 ≥4 次，
  qps 1033 → 1912、TTFT 714 → 1747ms，而**日志上看不出来**）。

### 改了 `k6.js` / `advanced/ab_topology.sh` / `lib/lib_rig.sh` 之后**必须**重新打封条

```sh
cd deploy/v88            # 下面都在 deploy/v88（包根）里执行
$EDITOR k6.js            # 或从仓库 scripts/loadtest/ 取新版覆盖；归因脚本是 advanced/ab_topology.sh
./seal_bundle.sh         # 刷新 BUNDLE_SHA256（封条里 ab_topology.sh 记的是 advanced/ 路径）
bash -c '. ./lib/lib_v88.sh; v88_check_bundle'   # 自证；preflight 每次也会跑
```

**漏了会怎样**：下一次 `run_full_88_xg.sh` 在 **preflight 直接 exit 2**、连农场都不碰 ——
**宁可开跑前红，也不要带着"文件名一样、内容不一样"的脚本去压一轮几十分钟**。

### 想核对"本目录这份"与仓库那份有没有漂移（可选，流程用）

仓库在手边时可以手动比一次 —— 但这**不是**门禁，只是给你一个一眼看漂移的办法：

```sh
# 在**仓库根**执行
diff -u scripts/loadtest/k6/k6.js deploy/v88/k6.js
```

## 3. 专用工具

**起环境与跑对标 → [`../STARTUP.md`](../STARTUP.md)**（不在这里重复）。

```sh
./farm_ceiling_scan.sh --combo B --levels 1000,2000,4000,8000   # 农场产能上限
./ab_topology.sh                                                # 瓶颈归因（网关 / 农场 / k6）
```

主驱动器的 `--preflight-only` / `--dry-run` **不需要**起 mock——农场可达性检查会因此失败，
这正是它该做的事（宁可在开跑前红，也不要跑完才发现口径是错的）。

### 先在现场测农场上限，再决定扩几台

你们的现场是 **10 GbE + 32 核 mock**：链路不再是墙（10 GbE ≈45000 流容量），
**农场就是墙**。仓库那套「34–38 核/千请求/秒」是在 128 核实验室机上测的，不能直接
拿来买机器。先实测：

```sh
./farm_ceiling_scan.sh --combo B --levels 1000,2000,4000,8000
```

判据**不依赖 ssh 采远端 CPU**（本仓 §13 已按评审删掉过一条跨机 coproc 的支路，
理由是用四段高风险代码换一条已知结论）：农场饱和的签名是
**VU 涨了但 qps 涨不动、而 fail 仍然是 0**（网关只是在排队，从不拒连）；
链路/网关到顶则表现为 fail 上升。连续两档 `qps` 涨幅 < 5% 即判到顶。

输出里有你机器的实测「核 / 千请求/秒」，以及**要冲目标档位需要几台 mock 机**。

> 要 mock 机的 CPU 曲线，就在 mock 机上自己跑一行（框架故意不做远端采样）：
> ```sh
> pid=$(pgrep -f mock-llm | head -1); \
> awk -v p="$pid" '{print systime(), $14, $15}' /proc/$pid/stat
> ```

### 两套试验必须分开跑

| | 对标 V88 | 产能 / 天花板 |
| --- | --- | --- |
| 驱动 | `run_full_88_xg.sh` | `farm_ceiling_scan.sh`（或 `ab_topology.sh`） |
| `KEEP_BODY` | `true`（要 `gw_oh`） | `false`（不解析 body，k6 不自锁） |
| `MOCK_BATCH` | **1** | 5 / 10（**台架口径**） |
| 看什么 | `ttft` / `dur` / `fail` / `gw_oh` / 活跃流 | `qps` 天花板 / 三组 CPU |

**不要把两套的数混在一张表里**（跨口径混算在 `v88-rig.md` §13.3 已犯过三次）。

## 4. 口径红线（三条不许做）

这三条不是风格问题，做了整个对标就作废：

1. **不许提前写 200 / 发 SSE 空头**。k6 的 TTFT 是 `timings.waiting`（第一个字节），
   空头会把排队从 `waiting` 挪进 `receiving`：TTFT 能报到 ~1ms（**低于 mock 的 500ms**），
   而 `dur` 不变、`gw_oh` 暴涨。它破坏 failover 与状态码透传，也违反
   [`docs/ai/agents/wire-boundary.md`](../../../docs/ai/agents/wire-boundary.md)。
2. **不许拿 `batch≠1` 的表当产品口径**，也不许与 `batch=1` 的表混算比值。
   真实上游按 token 推送 = 每 chunk 一次 flush = `batch=1`。
3. **不许只统计成功请求的分位、只给对手限流、或改了喂法却仍声称 checks 比 2.00**。
   延迟分位与 `fail%` 分开如实报；两侧同口径。

**合法且真实对 xlite 有利**的做法（建议全用）：0.3.0+ 镜像（`TCP_NODELAY` 内置，
`gw_oh` 全分位 −9%~−22%）、`TOKIO_WORKER_THREADS=8`、backlog 65535 + `somaxconn`、
不设 `XLITE_MAX_IN_FLIGHT`、HAProxy 探 `/health` 且 `timeout tunnel 600s`、
并把判别力放在 `gw_oh` 与高载 `fail%` 上——低载 TTFT 被 mock 的 500ms 定长支配，
两侧本就平局，在它上面争不出优势。

## 5. 现场清单

1. `docker load -i bench/0924/deploy_origin/deploy88/mock-llm-h2c.tar.gz` → `mock-llm:h2c`
   （`RepoTags: ["mock-llm:h2c"]`）。⚠️ **镜像里的 `/mock-llm` 与农场跑的不是同一份**：
   农场必须把包里的裸二进制 bind-mount 覆盖上去，只 `docker run mock-llm:h2c` 会静默换口径 ——
   那份没有 `ts`、没有 finish/usage chunk（`out=500` 只有 **501 行 / 91 014 B**，而口径是
   **502 行 / ≈100 736 B**），`ttft` 带 `×U(0.5,1.5)` 抖动、默认 `ttft=0 / batch=0`
   （裸二进制是精确 `ttft=500 / batch=1`）
2. 起 mock：**路数与 `MOCKS` 环境变量一致**（24 口 = `8081-8104`）、`--network host`、
   `--ulimit nofile=1048576:1048576`、`--entrypoint /mock-llm`、
   `-v …/mock-llm-linux-usage:/mock-llm:ro`（裸二进制 sha256 `dc0f4316…`）；
   客户现成的 `start-mocks.sh` 只有 **12** 口。完整命令与逐口回读见 [`../STARTUP.md`](../STARTUP.md) §1
3. `sysctl --system`（客户包里的 `sysctl-loadtest.conf`：`somaxconn=65535` 等）
4. 网关：`xlite:0.5.2`（不要 0.2.0）
5. HAProxy：`httpchk GET /health`（不是 `/api/status`）、`timeout client/server/tunnel 600s`
6. `run_full_88_xg.sh --preflight-only` → 红就先修，别开跑
7. 开跑；结果目录里的 `env.txt` 是**这一轮口径的唯一凭据**（含生效 `MOCK_BATCH`
   与农场原值 `MOCK_BATCH_BEFORE`）

> 每一项**具体填什么值、为什么**（`TOKIO_WORKER_THREADS` 的实测表与选法、HAProxy 三处必改、
> 内核 sysctl、以及"不要设什么"）都在 [`CONFIG.md`](CONFIG.md)。

> 从零把环境拉起来（mock 农场 / Redis 目录 / 网关 / HAProxy / k6）见 [`../STARTUP.md`](../STARTUP.md)。

> ⚠️ `kill -9` 下 `trap … EXIT` **不执行**，农场会停在改过的 `batch` 上。
> 所以下一轮开跑前的回读，比任何日志都可信。

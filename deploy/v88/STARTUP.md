# V88 启动与跑测（xlite 0.5.3）

本页用于现有 V88 测试环境。配置依据见 [`advanced/CONFIG.md`](advanced/CONFIG.md)，对标口径见 [`../../docs/manual/v88-rig.md`](../../docs/manual/v88-rig.md)。

| 机器 | 用途 | 端口 |
| --- | --- | --- |
| `10.56.88.22` | 现有 Redis + HAProxy | 6379 / 8005（双节点）/ 8006（单节点） |
| `10.56.88.24`、`10.56.88.26` | xlite，两台配置一致 | 8080 |
| `10.56.88.29` | 24 个 h2c mock + k6 | 8081–8104 |

链路：`k6 → HAProxy → xlite → h2c mock`。准备好两台网关上的 `xlite:0.5.4` 镜像，以及 `.29` 上的 mock 包和 k6 镜像。`deploy/v88/` 必须整目录拷贝。

## 1. 启动 mock（在 .29）

以下用于首次启动；已有 `mock51h-*` 容器时先确认是否需要重建，不要重复创建。

```bash
MOCK_BUNDLE=/path/to/mock-bundle   # 改成客户 mock 包目录
docker load -i "$MOCK_BUNDLE/mock-llm-h2c.tar.gz"
mkdir -p /root/loadtest/mock
cp "$MOCK_BUNDLE/mock-llm-linux-usage" /root/loadtest/mock/
chmod +x /root/loadtest/mock/mock-llm-linux-usage

for p in $(seq 8081 8104); do
  docker run -d --name "mock51h-$p" --network host --restart unless-stopped \
    --ulimit nofile=1048576:1048576 \
    -v /root/loadtest/mock/mock-llm-linux-usage:/mock-llm:ro \
    --entrypoint /mock-llm -e PORT="$p" mock-llm:h2c
done

sleep 2
OK=0
for p in $(seq 8081 8104); do
  curl -fsS -m 3 "http://127.0.0.1:$p/health" | grep -q ok && OK=$((OK+1))
done
echo "mock 健康: $OK/24"           # 必须是 24/24
```

## 2. Redis 与目录

- 用现有 `redis://10.56.88.22:6379`，**不要新起 Redis、不要改它的启动参数**。
- 当前不支持 `redis+sentinel://`。
- 目录已有时直接复用；确认通道指向 `.29:8081–8104`，记下模型名和有效 API Key。
- 空目录才按 [`advanced/CONFIG.md` §4.1](advanced/CONFIG.md#41-空-redis-时怎么灌目录) 导入。不要对已有目录使用 `--replace`，它会清空目录。

## 3. 启动网关（在 .24、.26 各执行一次）

两台使用**同一个管理员密码**。不设 `ADMIN_SESSION_SECRET` 时，会话密钥从密码派生；如另设该变量，两台也必须一致。

```bash
read -r -s -p '管理员密码（两台相同）: ' ADMIN_PASSWORD
printf '\n'
export ADMIN_PASSWORD

docker run -d --name xlite-gw --network host --restart unless-stopped \
  --ulimit nofile=1048576:1048576 \
  -e TOKIO_WORKER_THREADS=8 \
  -e REDIS_URL=redis://10.56.88.22:6379 \
  -e XLITE_UPSTREAM_H2C=1 \
  -e ADMIN_PASSWORD -e LISTEN_HOST=0.0.0.0 -e PORT=8080 \
  xlite:0.5.4

curl -fsS http://127.0.0.1:8080/health    # version 应为 0.5.3
curl -fsS http://127.0.0.1:8080/ready     # Redis 就绪
docker exec xlite-gw sh -c "grep -i 'open files' /proc/1/limits"
sysctl -n net.core.somaxconn
```

`nofile` 应为 `1048576`，`somaxconn` 应为 `65535`；不够时按 [`advanced/CONFIG.md` §3](advanced/CONFIG.md) 处理。

**当前设置：**

| 项 | 怎么设 |
| --- | --- |
| `TOKIO_WORKER_THREADS` | `8`，作为本环境起点，不代表所有负载的最优值 |
| `XLITE_UPSTREAM_H2C` | 本农场用 `1`；仅适用于支持 h2c 的明文上游 |
| `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` | 上面的命令不设：h2c 默认 `32`；明文 HTTP/1.1 默认 `128` |
| `XLITE_UPSTREAM_STREAM_CAP` | 不设，跟随对端；不要照搬旧配置里的 `30` |
| `XLITE_CATALOG_RELOAD_SECS` | 不设，默认 `5` 秒同步目录；仅固定目录压测时可设 `0` 冻结 |
| `XLITE_UPSTREAM_H1_POOL` / `XLITE_MAX_IN_FLIGHT` | 不要设，当前代码不再读取 |
| `XLITE_UPSTREAM_TTFT_HEADER` | 对标时不要开启 |

**若上游是普通 HTTP/1.1 而非 h2c**：把启动参数改为 `-e XLITE_UPSTREAM_H2C=0`，明文 `http://` 才会走 h1 池。镜像默认启用 h2c，仅删除参数不能关闭。显式设 `MAX_CONNS_PER_HOST=32` 会把 h1 也限制到 32，不能继承这项旧 h2c 配置。`https://` 仍走 reqwest，不适用上述池上限。

已有 `xlite-gw` 时，保留原有挂载和必要配置后重建容器；**`docker restart` 不会更新环境变量**。两台必须一起对齐。

## 4. HAProxy（在 .22）

在现有配置中使用以下入口和后端；不要覆盖同机其他服务的配置。

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
    option httpchk GET /health
    http-check expect status 200
    server n24 10.56.88.24:8080 check inter 5s fall 5 rise 2
```

检查入口 `http://10.56.88.22:8005/health` 和 `:8006/health` 都返回 200。
HAProxy 的实际 fd 上限也要核对，不能只看 `maxconn`；完整链路清单与 systemd 设置见 [`advanced/CONFIG.md` §3.1–3.2](advanced/CONFIG.md)。修改 `LimitNOFILE` 后须重启，不能只重载。

## 5. 跑测（在 .29 的 deploy/v88 目录）

```bash
cd /path/to/deploy/v88            # 改成实际拷贝目录
export TARGET=http://10.56.88.22:8005/v1/chat/completions
read -r -s -p '压测 API Key: ' TOKEN
printf '\n'
export TOKEN
export MODEL=mock-model           # 必须与目录里的模型名一致；§2 导入示例为 mock
export MOCKS=8081-8104
export K6_IMAGE=grafana/k6:latest  # 本机必须已有；离线先 docker load
export K6_MEM=45g                 # .29 历史实测约 61 GiB，还要留内存给 mock 和系统
export RESULT_DIR=/root/v88-run1  # 一张阶梯一个新目录，后续各组共用

./run_full_88_xg.sh --preflight-only
./run_full_88_xg.sh --combo B --levels 1000,4000   # 先小跑
RESUME=1 ./run_full_88_xg.sh --combo B             # 复用成功小跑档，补齐 B
./run_full_88_xg.sh --combo D
./run_full_88_xg.sh --combo E
./run_full_88_xg.sh --combo F
./run_full_88_xg.sh --combo A
./run_full_88_xg.sh --combo C
```

中断后用同一个 `RESULT_DIR`，执行 `RESUME=1 ./run_full_88_xg.sh --combo <被中断的组>`；只复用退出码为 `0` 的档。换组不需要 `RESUME`，改了配置或口径则必须换结果目录。

### 5.1 默认口径与注意事项

- 阶梯：`1000,2000,…,10000,15000,…,60000`，每组 20 档。要与客户逐档一致，**在所有轮次前** `export STEP_COARSE_ABOVE=0`；不要降低 `MAX_VUS`。
- 默认 `MOCK_BATCH=1`、`KEEP_BODY=true`；分组采样为 A/E=100%、B=50%、C/F=10%、D=5%。复现历史全量解析时显式 `export KEEP_SAMPLE=1`，两侧同口径重跑，注意内存压力。
- 默认 `GRACEFUL_STOP=900s`、`STREAM_TIMEOUT=600000`、`STOP_ON_FAIL=0`。客户历史收尾为 `600s`，没有超时余量；严格复刻时须披露此差异。
- 开跑前 `free -g`，不要在约 61 GiB 的 `.29` 上使用脚本默认 `K6_MEM=80g`。`45g` 是历史起点，高档仍可能 OOM，不是容量保证。
- driver 未设 `--memory-swap`。小跑期间用 `docker inspect <k6容器名> --format '{{.HostConfig.Memory}} / {{.HostConfig.MemorySwap}}'` 核对；可能使用 swap 时先解决，否则延迟数据可能失真。
- 结果看 `progress.log`、每档 `.log` / `.rc` / `.summary.json`。检查完成组标记、失败分类 `split(...)` 和降采样记录；`env.txt` 每轮覆盖，不要跨轮修改口径。

## 6. 有排队或首字慢时

压测进行中，在 `.24`、`.26` 各运行一次（网关机也需拷贝本包）：

```bash
cd /path/to/deploy/v88
read -r -s -p '管理员密码: ' ADMIN_PASSWORD
printf '\n'
export ADMIN_PASSWORD
./advanced/pool_sizing_probe.sh --gw-url http://127.0.0.1:8080 --seconds 120
```

探针优先读 `/api/pool`，0.5.3 同时覆盖 h2c 和 h1 池；API 不可用时回落 `ss`，所以在网关机上运行。退出码 `0` 才有有效判定，`2` 负载不足、`3` 缺工具、`4` 采样不可用。

先看上界、在途与排队，再决定是否改连接数；不要直接照搬旧的 `CAP=30 / CONNS=25` 或实验室建议 `CONNS=3`。两台保持一致，重建后用同档复测。历史性能变化仅为实验室结果，**客户环境未验证**；详见 [`advanced/CONFIG.md` §5.1](advanced/CONFIG.md)。

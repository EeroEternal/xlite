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

## 6. 核对清单

| # | 机器 | 做什么 | 怎么验证 |
| --- | --- | --- | --- |
| 1 | 22 | Redis 在跑（别人的，**别动**） | `redis-cli -h 10.56.88.22 ping` → `PONG`（或在 22 上 `docker exec redis redis-cli ping`） |
| 2 | 22 | HAProxy 在跑（cfg 见 §4） | `curl -s http://10.56.88.22:8005/health` → `{"status":"ok",…}` |
| 3 | 24 / 26 | 起 xlite（§3） | `curl -s http://10.56.88.24:8080/health`、`…/ready` |
| 4 | 29 | 起 24 个 mock（§1） | 脚本打印 `mock 健康: 24/24` |
| 5 | 29 | 下发口径 + 单流自检（§1） | 502 行 `data:` / ≈100 736 B / 500 个 `ts` |
| 6 | 29 | 前置检查 | `TARGET=… TOKEN=… ./run_full_88_xg.sh --preflight-only` → `RC=0` |
| 7 | 29 | 开跑 | `./run_full_88_xg.sh`；结果目录里看 `progress.log`（逐档一行）与 `env.txt`（口径凭据） |

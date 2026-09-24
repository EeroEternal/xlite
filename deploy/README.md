# 24 mock + Redis 部署（Docker 镜像）

本目录是 **24 上游 mock、Redis 目录、xlite 网关** 的操作入口。只用 Docker 镜像，不要在宿主机编译或调用仓库里的二进制产物。

没有 PostgreSQL。供应商 / 模型 / 通道 / API Key 写在 Redis 键 `xlite:catalog`。请求路径只读网关进程里的内存快照（约 5 秒从 Redis 拉一次）。

| 文件 | 作用 |
| --- | --- |
| `README.md` | 本文 |
| `docker-compose.yml` | redis + mocks + xlite（可选 xlite-2） |
| `env.example` | 可改的参数；复制为 `.env` |

镜像在仓库根构建：`Dockerfile`（网关）、`Dockerfile.mock`（24 端口 SSE）。目录 YAML 用 `examples/mock-24.docker.yaml`（上游主机名是 `mocks`）。

当前镜像标签：`xlite:0.3.0`、`xlite-mock:0.1.0`。

---

## 1. 拓扑

```
curl / k6 / 浏览器
        |  宿主机 :8080
        v
     xlite 容器  --明文 HTTP/2-->  mocks 容器 :4101 … :4124
        |
        +-- redis://redis:6379   键 xlite:catalog
```

Compose 服务名：

| 服务 | 镜像 | 宿主机端口 | 容器里别人怎么访问它 |
| --- | --- | --- | --- |
| `redis` | `redis:7-alpine` | 6379 | `redis:6379` |
| `mocks` | `xlite-mock:0.1.0` | 4101–4124 | `http://mocks:4101` … `http://mocks:4124` |
| `xlite` | `xlite:0.3.0` | 8080 | 网关自己 |
| `xlite-2`（`--profile ha`） | 同上 | 8081 | 第二台网关，同一 Redis |

调度固定为：对外模型名 `mock` → 24 条通道，权重 1、优先级 0 → 同一优先级内按权重随机，首字节前 5xx/429/连接失败才换下一条。一把 API Key，`models: []` 表示不限制模型。

**不要**把 `examples/mock-24.yaml`（`127.0.0.1`）导入进容器网关。容器里的 `127.0.0.1` 是网关自己，不是 mock 容器。必须用 `mock-24.docker.yaml`。

---

## 2. 参数说明

先复制环境文件：

```bash
cd ~/github/xlite/deploy
cp env.example .env
```

`docker compose` 会读同目录 `.env`。改完后对对应服务 `up -d`。

### 2.1 网关（服务 `xlite` / `xlite-2`）

| 变量 | 默认 | 24 mock 应设 | 含义 |
| --- | --- | --- | --- |
| `REDIS_URL` | 空 | **compose 已写** `redis://redis:6379` | 非空则目录走 Redis，忽略 SQLite |
| `XLITE_UPSTREAM_H2C` | 关 | `1` | 上游 URL 为 `http://` 时走明文 HTTP/2 |
| `ADMIN_PASSWORD` | `admin` | 自己改 | Admin 登录密码 |
| `ADMIN_SESSION_SECRET` | 空 | 可不设 | 签名 Admin 会话 Cookie 的密钥；不设则从 `ADMIN_PASSWORD` 派生。多节点必须一致 |
| `PORT` | `8080` | `8080` | **容器内**监听端口（映射仍是 8080:8080） |
| `LISTEN_HOST` | `0.0.0.0` | `0.0.0.0` | 必须全接口，否则宿主机进不去 |
| `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` | `32` | `32`，打满再加 | 每个上游 host 的 h2c 连接上限 |
| `DATABASE_URL` | `sqlite:xlite.db` | **不要设** | 有 Redis 时用不到 |

`XLITE_UPSTREAM_H2C` 为真的写法：`1`、`true`、`yes`、`on`（大小写不敏感）。

网关进程 listen backlog 是 65535，内核按 `net.core.somaxconn` 封顶。压 3 万连接时网关机执行：

```bash
sysctl -w net.core.somaxconn=65535
sysctl -w net.core.netdev_max_backlog=65535
sysctl -w net.ipv4.tcp_max_syn_backlog=65535
sysctl -w net.ipv4.ip_local_port_range="1024 65535"
```

不要设 `XLITE_MAX_IN_FLIGHT`。容器 `ulimit nofile` 至少 1048576（compose 外手跑 docker 时加 `--ulimit nofile=1048576:1048576`）。HAProxy 健康检查用 `GET /health`，不是 New API 的 `/api/status`。

h2c 池：对端 `SETTINGS_MAX_CONCURRENT_STREAMS` 满了会再拨一条连接，直到 `XLITE_UPSTREAM_MAX_CONNS_PER_HOST`。24 个 mock × 32 条约 768 条连接、按对端 250 流/连接估算约 19 万流上限，一般够本机压测。

### 2.2 mock 农场（服务 `mocks`，一个容器 24 个监听）

| 变量 | 冒烟 | B 组长流 | 含义 |
| --- | --- | --- | --- |
| `MOCK_MODEL` | `mock` | `mock` | 与 YAML `upstream_model` 一致 |
| `MOCK_CHUNKS` | `20` | `500` | 每个流的 SSE 块数 |
| `MOCK_INTERVAL_MS` | `10` | `10` | 块间隔 |
| `MOCK_TTFT_MS` | `0` | `0` | 第一个字节前额外等待 |
| `MOCK_LISTEN` | `0.0.0.0` | `0.0.0.0` | 必须 `0.0.0.0`，否则隔壁容器连不上 |

B 组墙钟大约 `500 × 10ms = 5s` 量级（再加网关和调度）。只改 mock 时长：

```bash
# 在 deploy/.env 把 MOCK_CHUNKS 改成 500，然后：
docker compose up -d mocks
```

### 2.3 Redis

Compose 里是 `redis:7-alpine`，`--save "" --appendonly no`（压测不落盘）。键：

- `xlite:catalog`：整份目录 JSON
- `xlite:catalog:lock`：写入时的短锁

没有别的 xlite 配置键。登录 session 是**无状态签名 Cookie**（HMAC-SHA256），不进 Redis；只要各节点 `ADMIN_PASSWORD`（或显式 `ADMIN_SESSION_SECRET`）一致，任意节点都能校验，HAProxy 轮询下也不用粘性。

---

## 3. 启动（从本目录）

机器需要：Docker Engine + Compose v2。在 **仓库的 `deploy/`**：

```bash
cd ~/github/xlite/deploy
cp -n env.example .env
docker compose up -d --build
docker compose ps
```

`--build` 第一次必加。之后只改环境变量可以不加。

等到 `redis`、`mocks`、`xlite` 都是 running，且 health 为 healthy。`mocks` 要拉起 24 个 Node 进程，可能要十几秒。

只构建镜像、不启动：

```bash
cd ~/github/xlite
docker build -t xlite:0.3.0 .
docker build -f Dockerfile.mock -t xlite-mock:0.1.0 .
```

---

## 4. 导入 24 路目录

网关起来之后，用**一次性容器**往 Redis 写 YAML（镜像里已带文件）：

```bash
cd ~/github/xlite/deploy
docker compose run --rm xlite import /opt/xlite/examples/mock-24.docker.yaml --replace
```

示例 YAML 已写死客户端密钥，不必开 Admin：

```yaml
api_keys:
  - name: loadtest
    key: sk-loadtest
    models: []
```

import 成功后 curl / k6 直接用 `sk-loadtest`。要换一把，改 YAML 里的 `key:` 再 import。不写 `key` 时才会现场生成，并在终端打印一次；生成之后界面里也只能看到掩码，看不到完整明文。

`--replace`：先清空 Redis 目录再写入。合并（按名称更新、保留未出现的对象）去掉 `--replace`。

确认 Redis 里有 24 个 provider：

```bash
docker compose exec redis redis-cli GET xlite:catalog | grep -c mock-41
```

应接近 24。网关最多约 5 秒后内存快照与 Redis 一致，再打流量。

---

## 5. 冒烟

```bash
export KEY=sk-loadtest

curl -sS http://127.0.0.1:8080/health
# {"status":"ok","gateway":"xlite"}

curl -sS http://127.0.0.1:8080/ready
# {"status":"ready"}   否则 Redis 没通

curl -sS -H "Authorization: Bearer $KEY" http://127.0.0.1:8080/v1/models
# data 里有 "id":"mock"

curl -N -sS \
  -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"mock","stream":true,"messages":[{"role":"user","content":"hi"}]}' \
  http://127.0.0.1:8080/v1/chat/completions
```

最后一条应持续刷 `data: {...}`。宿主机探 mock：

```bash
curl -sS http://127.0.0.1:4101/health
```

Admin：浏览器打开 http://127.0.0.1:8080  
用户名任意，密码为 `.env` 里的 `ADMIN_PASSWORD`。设置页「配置存储」应为「Redis（集群共享）」，供应商 24 条。

---

## 6. 压测（k6）

1. `.env` 设 `MOCK_CHUNKS=500`、`MOCK_INTERVAL_MS=10`，然后 `docker compose up -d mocks`。
2. 连接打满再加大网关：`.env` 里 `XLITE_UPSTREAM_MAX_CONNS_PER_HOST=64`，`docker compose up -d xlite`。
3. k6 目标：`http://127.0.0.1:8080/v1/chat/completions`  
   Header：`Authorization: Bearer $KEY`  
   Body：`{"model":"mock","stream":true,"messages":[{"role":"user","content":"hi"}]}`

---

## 7. 第二台网关

同一份 Redis，不必再 import：

```bash
cd ~/github/xlite/deploy
docker compose --profile ha up -d xlite-2
```

宿主机 `:8081`。等最多 5 秒后：

```bash
curl -sS -H "Authorization: Bearer $KEY" http://127.0.0.1:8081/v1/models
```

两个网关共用同一份 `ADMIN_PASSWORD`，Admin 会话是无状态签名 Cookie，`:8080` 登录后直接访问 `:8081` 也是登录态（有负载均衡时同样无需粘性）。

---

## 8. 不用 Compose 的等价命令

```bash
docker network create xlite-net

docker run -d --name xlite-redis --network xlite-net -p 6379:6379 redis:7-alpine

docker run -d --name xlite-mocks --network xlite-net --network-alias mocks \
  -p 4101-4124:4101-4124 \
  -e MOCK_MODEL=mock -e MOCK_CHUNKS=20 -e MOCK_INTERVAL_MS=10 -e MOCK_LISTEN=0.0.0.0 \
  xlite-mock:0.1.0

docker run --rm --network xlite-net \
  -e REDIS_URL=redis://xlite-redis:6379 \
  xlite:0.3.0 import /opt/xlite/examples/mock-24.docker.yaml --replace

docker run -d --name xlite-gw --network xlite-net -p 8080:8080 \
  -e REDIS_URL=redis://xlite-redis:6379 \
  -e XLITE_UPSTREAM_H2C=1 \
  -e ADMIN_PASSWORD=admin \
  -e LISTEN_HOST=0.0.0.0 \
  xlite:0.3.0
```

`--network-alias mocks` 不能省，否则 YAML 里的主机名 `mocks` 解析不到。

---

## 9. mock 在另一台机器

YAML 不能再用主机名 `mocks`。复制 `examples/mock-24.docker.yaml`，把 `http://mocks:` 换成 mock 机可达地址，例如 `http://10.0.0.2:`，再：

```bash
docker compose run --rm -v /path/to/custom.yaml:/tmp/cat.yaml:ro \
  xlite import /tmp/cat.yaml --replace
```

网关机：只跑 `redis` + `xlite`（可把 compose 里 `mocks` 注释掉）。mock 机：只跑 `xlite-mock` 镜像并映射 4101–4124。`REDIS_URL` 指向网关机上的 Redis（注意跨机用宿主机 IP，不要用服务名 `redis`）。

---

## 10. 日志与排障

```bash
docker compose logs -f xlite
docker compose logs --tail=80 mocks
docker compose exec redis redis-cli PING
```

xlite 启动日志应含 `catalog backend redis` 且 `h2c=true`。

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| 502 / 上游连不上 | 导入了 `127.0.0.1` 的 YAML | 用 `mock-24.docker.yaml` 再 `--replace` |
| `/ready` 503 | Redis URL 写成了 `127.0.0.1` | 容器内必须是 `redis://redis:6379` |
| mock 健康检查失败 | 监听 `127.0.0.1` | `MOCK_LISTEN=0.0.0.0` |
| Admin 空白 | 镜像没打进 `admin/dist` | 在仓库根用 `Dockerfile` 重新 `--build` |
| 第二台没有模型 | 快照未 reload | 等 5 秒；两台 `REDIS_URL` 必须相同 |
| 401 | YAML 没写 `key` 或和请求不一致 | 示例用 `sk-loadtest`；改 YAML 后重新 import |

---

## 11. 停止

```bash
cd ~/github/xlite/deploy
docker compose --profile ha down
```

只要清目录、服务继续跑：

```bash
docker compose exec redis redis-cli DEL xlite:catalog xlite:catalog:lock
```

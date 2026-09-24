# 24 mock + Redis：Docker 操作说明

本页只讲 **Docker 镜像** 怎么配、怎么起、参数怎么设。不要用本机 `cargo` 产物。配置走 Redis，没有 PostgreSQL。

适用版本：镜像标签 `xlite:0.1.0`（与 `Cargo.toml` 一致）。

## 1. 这套台架有什么

同一条 Docker 网络里三个角色：

| 服务名 | 镜像 | 作用 |
| --- | --- | --- |
| `redis` | `redis:7-alpine` | 目录源。键 `xlite:catalog` |
| `mocks` | `xlite-mock:0.1.0` | 一个容器内 24 个 SSE 上游，端口 4101–4124 |
| `xlite` | `xlite:0.1.0` | 网关。读 Redis，h2c 转发到 `mocks` |

客户端（curl / k6 / Admin）打宿主机 `8080`。网关在容器里用主机名 `mocks` 访问上游，YAML 必须写 `http://mocks:4101`，不能写 `127.0.0.1`。

调度：一个对外模型名 `mock`，24 条通道，权重 1、优先级 0；一把 API Key（`models: []` 表示全部模型）。

```
k6/curl  -->  :8080 xlite  --h2c-->  mocks:4101..4124
                 |
                 +--> redis://redis:6379  (xlite:catalog)
```

## 2. 构建镜像

在仓库根目录：

```bash
cd ~/github/xlite

docker build -t xlite:0.1.0 .
docker build -f Dockerfile.mock -t xlite-mock:0.1.0 .
```

`xlite:0.1.0` 里已经带上 Admin 静态资源和 `examples/`。容器工作目录是 `/opt/xlite`，入口是 `xlite`。

无参启动网关：

```bash
docker run --rm xlite:0.1.0
```

导入目录（一次性容器，跑完退出）：

```bash
docker run --rm xlite:0.1.0 import /opt/xlite/examples/mock-24.docker.yaml --replace
```

导出：

```bash
docker run --rm xlite:0.1.0 export
```

## 3. 环境变量

网关容器（`xlite` / `xlite-2`）认这些变量。Compose 已写好 24 mock 场景的推荐值。

| 变量 | 24 mock 推荐值 | 说明 |
| --- | --- | --- |
| `REDIS_URL` | `redis://redis:6379` | 一设就走 Redis 目录。Compose 服务名是 `redis` |
| `XLITE_UPSTREAM_H2C` | `1` | 上游 `http://` 走明文 HTTP/2。真值：`1` / `true` / `yes` / `on` |
| `ADMIN_PASSWORD` | 自己设一个 | 控制台登录密码。默认 `admin`，仅本机可接受 |
| `PORT` | `8080` | 容器内监听端口 |
| `LISTEN_HOST` | `0.0.0.0` | 必须是 `0.0.0.0`，否则宿主机映射进不来 |
| `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` | `32` | 每个上游 host 的 h2c 连接上限。对端流满了再拨新连接 |
| `DATABASE_URL` | 不要设 | 有 `REDIS_URL` 时不会用 SQLite |

mock 容器（`mocks`）：

| 变量 | 冒烟 | B 组（约 5–7s 流） |
| --- | --- | --- |
| `MOCK_MODEL` | `mock` | `mock`，与 YAML 里 `upstream_model` 一致 |
| `MOCK_CHUNKS` | `20` | `500` |
| `MOCK_INTERVAL_MS` | `10` | `10` |
| `MOCK_TTFT_MS` | `0` | `0`（需要人为拉长首字节再改） |
| `MOCK_LISTEN` | `0.0.0.0` | 必须 `0.0.0.0`，否则 xlite 容器连不上 |

改 mock 参数要重建或重启 `mocks` 容器。网关参数改环境变量后重启 `xlite`。

## 4. 一键拉起

```bash
cd ~/github/xlite
docker compose -f docker-compose.mock-24.yml up -d --build
```

等 healthy：

```bash
docker compose -f docker-compose.mock-24.yml ps
```

`redis`、`mocks`、`xlite` 都应是 running / healthy。

## 5. 把 24 路目录写入 Redis

必须用 **Docker 网络里的 YAML**（`examples/mock-24.docker.yaml`，上游是 `http://mocks:端口`）。`examples/mock-24.yaml` 写的是 `127.0.0.1`，只给网关跑在宿主机时用。

```bash
docker compose -f docker-compose.mock-24.yml run --rm xlite \
  import /opt/xlite/examples/mock-24.docker.yaml --replace
```

终端会打印类似：

```
imported providers +24/~0 models +1/~0 channels +24/~0 keys +1/~0
api key loadtest: sk-................................
```

把 `sk-` 那一行整段复制下来。明文只出现这一次。

`--replace` 会清空 Redis 里现有目录再写入。只想按名称更新、不删旧数据，去掉 `--replace`。

核对 Redis：

```bash
docker compose -f docker-compose.mock-24.yml exec redis redis-cli GET xlite:catalog | head
```

应能看到 24 个 `mock-4101` … `mock-4124`。

网关每约 5 秒从 Redis reload 内存快照。import 之后等几秒再打流量。

## 6. 冒烟

把上一步的 Key 放到环境变量：

```bash
export KEY='sk-粘贴完整密钥'
```

健康检查：

```bash
curl -sS http://127.0.0.1:8080/health
curl -sS http://127.0.0.1:8080/ready
```

`/ready` 要对 Redis `PING` 成功才 200。

模型列表（应含 `mock`）：

```bash
curl -sS -H "Authorization: Bearer $KEY" http://127.0.0.1:8080/v1/models
```

流式补全（应刷出 SSE `data:` 行）：

```bash
curl -N -sS \
  -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"mock","stream":true,"messages":[{"role":"user","content":"hi"}]}' \
  http://127.0.0.1:8080/v1/chat/completions
```

Admin：浏览器打开 http://127.0.0.1:8080 ，用户名任意、密码为 `ADMIN_PASSWORD`。设置页「配置存储」应为 Redis。供应商列表应有 24 条。

从宿主机探一个 mock：

```bash
curl -sS http://127.0.0.1:4101/health
```

## 7. 切到 B 组时长（500 × 10ms）

默认 Compose 是冒烟（20 chunk）。压测前：

```bash
MOCK_CHUNKS=500 MOCK_INTERVAL_MS=10 docker compose -f docker-compose.mock-24.yml up -d mocks
```

或改 `docker-compose.mock-24.yml` 里 `mocks.environment.MOCK_CHUNKS` 为 `"500"` 再 `up -d mocks`。

连接池：24 个上游、每个最多 32 条 h2c 连接，默认够用。打满后再加大：

```bash
XLITE_UPSTREAM_MAX_CONNS_PER_HOST=64 docker compose -f docker-compose.mock-24.yml up -d xlite
```

k6 打 `http://127.0.0.1:8080/v1/chat/completions`，Header `Authorization: Bearer $KEY`，JSON 里 `"model":"mock"`、`"stream": true`。

## 8. 第二台网关（同一份 Redis）

```bash
docker compose -f docker-compose.mock-24.yml --profile ha up -d xlite-2
```

`xlite-2` 映射宿主机 `8081`。不必再 import，读的是同一把 `xlite:catalog`。最多约 5 秒后 `/v1/models` 与第一台一致。

登录 cookie 不进 Redis，两个 Admin 要分别登录。

## 9. 手工 `docker run`（不用 Compose 时）

先建网络：

```bash
docker network create xlite-net
```

Redis：

```bash
docker run -d --name xlite-redis --network xlite-net -p 6379:6379 redis:7-alpine
```

Mocks：

```bash
docker run -d --name xlite-mocks --network xlite-net --network-alias mocks \
  -p 4101-4124:4101-4124 \
  -e MOCK_MODEL=mock -e MOCK_CHUNKS=20 -e MOCK_INTERVAL_MS=10 \
  xlite-mock:0.1.0
```

`--network-alias mocks` 必须有，YAML 里的主机名才是 `mocks`。

导入：

```bash
docker run --rm --network xlite-net \
  -e REDIS_URL=redis://xlite-redis:6379 \
  xlite:0.1.0 import /opt/xlite/examples/mock-24.docker.yaml --replace
```

网关：

```bash
docker run -d --name xlite --network xlite-net -p 8080:8080 \
  -e REDIS_URL=redis://xlite-redis:6379 \
  -e XLITE_UPSTREAM_H2C=1 \
  -e ADMIN_PASSWORD=admin \
  xlite:0.1.0
```

## 10. 常见失败

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| import 成功但请求 502 | YAML 用了 `127.0.0.1` | 改用 `mock-24.docker.yaml` 再 `--replace` |
| `/ready` 503 | Redis 连不上 | `REDIS_URL` 在 Compose 里必须是 `redis://redis:6379`，不是 `127.0.0.1` |
| mock health 失败 | 监听了 `127.0.0.1` | `MOCK_LISTEN=0.0.0.0` |
| Admin 打不开 | 镜像构建时没打进 `admin/dist` | 用本仓库 `Dockerfile` 重新 `docker build` |
| 第二台网关没有模型 | 还没 reload | 等 5 秒；确认两台都是同一 `REDIS_URL` |
| 401 | Key 抄少了或 import 后又 `--replace` 生成了新 Key | 重新 import，用最新打印的 Key |

看日志：

```bash
docker compose -f docker-compose.mock-24.yml logs -f xlite
docker compose -f docker-compose.mock-24.yml logs --tail=50 mocks
```

启动日志里应有 `catalog backend redis` 和 `h2c=true`。

## 11. 停掉并清目录

```bash
docker compose -f docker-compose.mock-24.yml down
docker compose -f docker-compose.mock-24.yml exec redis redis-cli DEL xlite:catalog xlite:catalog:lock
```

`down` 之后 Redis 容器删除，目录一并没了。只想清目录、服务继续跑，执行 `DEL` 那一行即可。

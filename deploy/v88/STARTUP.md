# deploy/v88 —— 测试环境启动

口径见 [`../../docs/manual/v88-rig.md`](../../docs/manual/v88-rig.md)，各配置项取值见 [`CONFIG.md`](CONFIG.md)。

```
k6 ──HTTP/1.1──► HAProxy :8005/:8006 ──► xlite ──h2c──► mock-llm :8081-8104
                                            └─ Redis：目录快照（请求路径不碰）
```

## 1. mock 农场（8081-8104）

```sh
docker load -i bench/0924/deploy_origin/deploy88/mock-llm-h2c.tar.gz

mkdir -p /root/loadtest/mock
cp -f "$(dirname "$0")/mock-llm-linux-usage" /root/loadtest/mock/
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

下发口径 + 自检（单流应得 502 行 `data:` / ≈100 736 B / 500 个 `ts`）：

```sh
for p in $(seq 8081 8104); do curl -s -m 3 -X POST \
  "http://127.0.0.1:$p/_config?ttft=500&token_interval=10&out_tokens=500&mix=0&mode=2&batch=1" >/dev/null; done
```

## 2. Redis + 目录

```sh
docker run -d --name xlite-redis --network host redis:7-alpine redis-server --save "" --appendonly no

docker cp cat.yaml xlite-gw:/tmp/cat.yaml                      # upstream 指向 mock 机 :8081-8104
docker exec xlite-gw xlite import /tmp/cat.yaml --replace
```

## 3. 网关

```sh
docker run -d --name xlite-gw --network host --ulimit nofile=1048576:1048576 \
  -e TOKIO_WORKER_THREADS=8 -e REDIS_URL=redis://10.56.88.22:6379 \
  -e XLITE_UPSTREAM_H2C=1 -e XLITE_UPSTREAM_MAX_CONNS_PER_HOST=32 \
  -e ADMIN_PASSWORD='<自己定>' -e LISTEN_HOST=0.0.0.0 -e PORT=8080 \
  xlite:0.4.0
```

## 4. HAProxy（多节点才需要）

```haproxy
defaults
    mode http
    option http-keep-alive
    timeout client 600s
    timeout server 600s
    timeout tunnel 600s
backend xlite
    option httpchk GET /health          # 不是 /api/status
    server node1 10.0.0.1:8080 check inter 5s fall 3 rise 2
```

## 5. 跑

```sh
cd deploy/v88
export TARGET=http://<入口>/v1/chat/completions TOKEN=<key>
./run_full_88_xg.sh
```

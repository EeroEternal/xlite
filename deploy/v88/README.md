# deploy/v88 —— V88 压测包

**部署/跑测只需要看 [`STARTUP.md`](STARTUP.md)**（起 mock 农场 → Redis → 网关 → HAProxy → 跑）。

| 要干的事 | 去哪 |
| --- | --- |
| 起环境、跑对标 | [`STARTUP.md`](STARTUP.md) + [`run_full_88_xg.sh`](run_full_88_xg.sh) |
| 起 / 扩 / 查 mock 农场（口数 = `MOCKS`） | [`start_farm.sh`](start_farm.sh) |
| 现场配置细节（`TOKIO_WORKER_THREADS`、HAProxy、内核、不要设什么） | [`advanced/CONFIG.md`](advanced/CONFIG.md) |
| 测农场上限（决定加几台 mock 机） | [`advanced/farm_ceiling_scan.sh`](advanced/farm_ceiling_scan.sh) |
| 瓶颈归因（网关 / 农场 / k6 各吃多少核） | [`advanced/ab_topology.sh`](advanced/ab_topology.sh) |
| 框架为什么这么设计、封条规则、目录说明 | [`advanced/README.md`](advanced/README.md) |

本目录可**整目录拷到现场机**（离线、无 git、无仓库）直接跑。
`k6.js` / `lib/lib_rig.sh` / `advanced/ab_topology.sh` 由 `BUNDLE_SHA256` 自封条保护（preflight 每次校验）；
改过这三个文件要跑 `./seal_bundle.sh` 重打封条。

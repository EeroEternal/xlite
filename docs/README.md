# xlite docs

| Doc | What it is |
| --- | --- |
| [getting-started.md](getting-started.md) | Run, import a catalog, call `/v1` |
| [architecture.md](architecture.md) | Data plane, catalog, routing |
| [changelog.md](changelog.md) | Released versions |
| [design.md](design.md) | Admin UI design entry |
| [manual/xlite.md](manual/xlite.md) | Operator env, YAML, HTTP/2 |
| [manual/lab-loadtest.md](manual/lab-loadtest.md) | Mock-24、实验室两台机、客户现场拓扑 |
| [manual/v88-rig.md](manual/v88-rig.md) | 与 New API V88 对标的 k6 喂法 |
| [dev/loadtest-newapi-controlled-2026-09.md](dev/loadtest-newapi-controlled-2026-09.md) | 受控对照：客户 New API 镜像在实验台的复现与更正 |
| [dev/loadtest-dual-0927-vs-0924-2026-09.md](dev/loadtest-dual-0927-vs-0924-2026-09.md) | 双节点对标实测：0927 轮 XG vs 0924 轮 New API |
| [dev/loadtest-xg-0928-vs-newapi-0925-2026-09.md](dev/loadtest-xg-0928-vs-newapi-0925-2026-09.md) | 0928 轮 XG 新 rig 单节点 vs 0925~0928 轮 New API 双节点：B 组对照（吞吐 +37~42%、真实并发 2×） |
| [dev/loadtest-bottleneck-cpu-affinity.md](dev/loadtest-bottleneck-cpu-affinity.md) | 现场瓶颈分析与 CPU 隔离/绑核（Affinity）指南 |
| [dev/loadtest-execution-plan-2026-09-28.md](dev/loadtest-execution-plan-2026-09-28.md) | 实验台对标验证操作方案（SOP） |
| [dev/loadtest-report-2026-09-28.md](dev/loadtest-report-2026-09-28.md) | 现场压测与性能天花板对标实测报告（0.5.1 + STREAM_CAP=50，45k 吞吐 2.13×，农场 48k 饱和） |
| [../deploy/README.md](../deploy/README.md) | 24 mock + Redis，Docker 镜像启动 |

Agent rules live in the repo-root `AGENTS.md`.

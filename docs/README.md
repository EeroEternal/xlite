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
| [dev/loadtest-report-2026-09-29.md](dev/loadtest-report-2026-09-29.md) | 现场全量压测基准与深度性能分析报告（6 大场景 110 组阶梯全量数据，组 A 顶出 3,355 QPS / 112 万请求，长短流优化空间剖析） |
| [dev/loadtest-df-data-appendix-2026-10.md](dev/loadtest-df-data-appendix-2026-10.md) | D/F 战役实测数据全录（44 轮 k6 全指标、帧级证据、资源账、构建/配置指纹） |
| [dev/loadtest-df-optimization-plan-2026-10.md](dev/loadtest-df-optimization-plan-2026-10.md) | D/F 长流优化方案：取消传播、连接分散实验、资源观测与验收标准（P0~P3 与 §8 四轮验收已执行，数据见附录） |
| [dev/performance-tuning-guide.md](dev/performance-tuning-guide.md) | Xlite 高并发与极限负载调优实战指南（参数矩阵、长短流策略、四层链路协同与打擂话术） |
| [dev/rfc-adaptive-h2-pool-architecture.md](dev/rfc-adaptive-h2-pool-architecture.md) | RFC: HTTP/2 自适应最小负载连接池架构设计方案（P2C 调度、Soft-Cap 异步预热、长短流隔离，供外部专家审计） |
| [../deploy/README.md](../deploy/README.md) | 24 mock + Redis，Docker 镜像启动 |

Agent rules live in the repo-root `AGENTS.md`.

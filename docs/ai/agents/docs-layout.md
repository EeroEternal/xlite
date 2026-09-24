# Documentation layout rules

## 十一、文档目录与命名规范

`docs/` 必须按用途归档，禁止把草稿、手册和 AI 指南混放在根目录或随意新建专题子目录：

1. **`docs/ai/`**：面向代码代理的协作说明（如 `claude.md`）。仓库根只保留薄入口 `AGENTS.md`；其它 AI / LLM 指南必须放在 `docs/ai/`。`AGENTS.md` 分章细则在 `docs/ai/agents/`。
2. **`docs/dev/`**：开发中设计稿、适配说明、roadmap、进度与分析草案。尚未落地的计划可放这里；**已落地结论应写入 `docs/changelog.md` 或稳定入口文档，再删除过时草案**，禁止再维护第二份「项目快照」。
3. **`docs/architecture/`**：已落地子系统的架构深度参考（由 `architecture.md` 索引）；设计稿仍进 `docs/dev/`，落地稳定并经代码核证后归位至此。
4. **`docs/manual/`**：产品手册、运维操作、验收清单、管理员指南。
5. **`docs/market/`**：售前 / 竞品对照。只放对外可讲的产品对比，不放未落地设计、不放内部排期。

`docs/` 根目录只保留稳定宏观入口：`README.md`、`architecture.md`、`getting-started.md`、`design.md`（UI 规范唯一入口）、`changelog.md`。UI 分章正文在 [`docs/design/`](../../design/)，不得在 `docs/` 根再堆平行设计长文。产品定位与能力总览见 [`docs/manual/xgateway.md`](../../manual/xgateway.md)。细则见 [`docs/rules/documentation.md`](../../rules/documentation.md)。

文档文件名必须简短且可读，建议使用 kebab-case，长度控制在 **32 个字符以内（不含扩展名）**；禁止使用冗长文件名（如超过 40 字符）导致路径可读性和维护性下降。

单个文档文件原则上不得超过 **600 行**；接近或超过该阈值时，必须按主题拆分为多个文件，并在上层索引文档中补充链接，禁止继续把内容堆叠进同一个超长文件。

废弃设计稿、已失效验收记录、与本仓库无关的外部 API 抄本应直接删除，不得以“归档”名义堆积。

当前版本、近期主线与发版说明以 **`Cargo.toml` + `git log` + `docs/changelog.md`** 为准，不要另建易过期的仓库状态摘要。

---

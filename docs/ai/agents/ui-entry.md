# UI design entry rules

## 零、UI 设计规范入口（`docs/design.md`）

`docs/design.md` 是本项目的**UI / 视觉 / 交互统一设计规范入口**（薄索引：How agents use this + Overview + Hard rules + PR checklist）。分章正文在 [`docs/design/`](../../design/)。三层分工：入口写判断（读者任务、层级、反模式名）；`tokens.md` / `index.css` / `components/ui` 是词表；skill 与 PR checklist 是机械检查。职责相当于设计侧的 `AGENTS.md`：

1. `AGENTS.md` 约束代理**如何协作、如何验证、如何报告**。
2. `docs/design.md` + `docs/design/*` 约束代理**界面应该长什么样、如何保持视觉一致性**。

当任务涉及以下任一内容时，代理必须遵循上述设计规范，不得仅凭通用前端经验或历史页面印象擅自发挥：

1. Admin UI / 前端页面 / 组件样式调整
2. 颜色、字体、圆角、间距、阴影、布局、响应式、暗色模式
3. 卡片、表格、表单、Dialog、Badge、Button 等组件视觉规范
4. 新页面、新模块、新空状态、新图表卡片的视觉落地
5. 任何“看起来不统一”“缺少设计依据”“希望和现有后台风格一致”的请求

执行要求：

1. 做 UI 相关任务前，先走 skill [`admin-ui-change`](../../../.agents/skills/admin-ui-change/SKILL.md)：先读薄入口 `docs/design.md`，再**按需**打开 `docs/design/` 下对应分章，**禁止**一次加载全部设计文档；权威是入口 + 分章，不是 skill 正文。
2. 若设计规范与局部历史实现不一致，优先识别为“历史样式债务”或“实现漂移”，而不是擅自继续复制旧样式。
3. 若用户要求生成或修改 UI，但代理尚未按上一款加载对应分章，必须先补读再行动，不得跳过。
4. 在交付 UI 变更时，应明确说明本次实现遵循了哪些分章；若有偏离，必须说明原因。
5. 若任务明确要求更新视觉规范本身，应修改入口或对应分章（并视需要更新 `admin-ui-change` 路由表），避免代码与规范再次分叉。

### 全局配置入口规则

系统级或跨领域共享的全局配置，必须统一收纳在 Admin 的「设置」页面内，
不得在侧边栏或其它一级导航中新增与「设置」并列的独立配置入口。
业务页面可以展示当前全局配置状态和跳转入口，但不得复制一套全局配置表单，避免配置
来源不唯一。新增全局配置能力时，应先扩展「设置」页面的信息架构，再实现具体表单。

### Admin 多语言（i18n）与语言纯洁性

Admin 面向中英双语。凡用户可见文案（标签、按钮、占位符、空状态、提示、错误、dialog 说明等）：

1. 必须使用 `t('namespace.key')`（或项目既有 `t` 封装），不得把某一种语言字符串硬编码在组件里当唯一文案。
2. 新增或修改 key 时，**同时**更新 `admin/src/lib/i18n/locales/zh.ts` 与 `admin/src/lib/i18n/locales/en.ts`。
3. `t(key, fallback)` 的英文 fallback **不能**代替 locale 条目；fallback 仅作开发期兜底，合入前两侧 locale 必须齐全。
4. 产品语义变更时（例如「一 Key 一路由」改为「一 Key 多路由」），同步改两侧文案，避免 UI 控件已变、说明仍写旧模型。
5. **严禁中英文混杂**：中文环境展示纯中文，英文环境展示纯英文。除行业标准不可翻译专有名词（如 API Key, HTTP, JSON, Token, ID 等）外，禁止在中文词汇后附加括号英文（例如 ❌ `上游模型名 (Upstream Model)` 需改为 ✅ `上游模型名`；❌ `优先级 (Priority)` 需改为 ✅ `优先级`；❌ `权重 (Weight)` 需改为 ✅ `权重`）。
6. **标识符与卡片副标题**：卡片或列表中展示的技术标识符统一使用标准的英文 code slug（如 `ai-proxy`），不得在中文主标题下硬编码大写英文别名（如 ❌ `AI PROXY`）。

### 列表排序与检索

Admin 实体列表（供应商、模型、API Key 等）的排序与检索必须对用户可预期。

**排序**

1. 排序选项文案必须标明**方向**，不能只写字段名（❌「排序：价格」→ ✅「排序：价格（低→高）」）。
2. 优先用语义方向而非抽象「升序/降序」：名称 `A→Z`、优先级/数量 `高→低` 或 `多→少`、价格 `低→高`、时间 `新→旧`。
3. 同一字段在不同页面方向应一致（例如「创建时间」默认均为新→旧）。
4. 新增排序选项时，在组件内写清 comparator 逻辑，并在 `zh.ts` / `en.ts` 同步更新 label。
5. 参考实现：`admin/src/pages/models.tsx`（排序选项带方向，检索覆盖对外名称、供应商、上游模型名）。

**检索**

1. 搜索框 placeholder 必须说明**可搜字段**（参考 `providers.searchPlaceholder`、`keys.searchPlaceholder`），禁止泛化 placeholder「搜索」当唯一说明。
2. 过滤逻辑必须与 placeholder 声明的字段一致；不得 placeholder 写「项目」但实现未搜项目。
3. 检索通常大小写不敏感、子串匹配；多字段 OR 组合时在 placeholder 用顿号/逗号列举。
4. 新增列表页时复制 canonical list 的 toolbar 模式（见 `docs/design/components.md` Entity list pattern），而非另起 placeholder 格式。

边界说明：

1. 设计规范主要约束视觉与交互一致性，不替代业务逻辑、接口协议、数据库设计、队列算法等工程规范。
2. 非 UI 任务无需机械引用设计文档；但只要输出会影响用户可见界面，就应回到 `docs/design.md` 入口并按需打开分章校准。

---

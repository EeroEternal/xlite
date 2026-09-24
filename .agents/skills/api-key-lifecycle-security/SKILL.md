---
name: api-key-lifecycle-security
description: Financial-grade API key lifecycle security standards. Covers irreversible hash storage, single-reveal upon create/rotate in modal dialogs with copy/download, and masked read-only display. Use when designing, reviewing, or implementing API key management, rotation, creation, display, or storage.
---

# API Key 生命周期与金融合规安全规范 (API Key Lifecycle Security)

## 核心原则 (Core Tenets)

在银行、金融机构以及高等级数据合规要求（如 PCI-DSS / 等保三级）下，API 密钥具有凭证（Credential）等同效力。必须遵循以下黄金原则：

1. **不可逆存储 (Irreversible Hash Storage)**：
   - 数据库**严禁**明文存储 API 密钥。
   - 数据库仅保存密钥的单向加盐哈希（如 SHA-256 / PBKDF2 / Argon2）。
   - 任何内部人员、只读数据库备份、SQL 注入攻击均无法还原任何现存的有效通信密钥。

2. **单次展示 (Single Reveal Principle)**：
   - 完整明文密钥**仅且仅在**创建（Create）或重置（Rotate）成功的瞬间返回给前端一次。
   - 前端接收到明文后，必须通过**模态弹窗 (Modal Dialog)** 明确展示，不得作为页面底部普通横幅或静默展示。
   - 弹窗必须标配：
     - **完整明文展示**（支持等宽排版与一键复制）。
     - **下载 Key 文件 (.txt)** 能力（便于用户保存至本地密钥库或配置文件）。
     - **一键复制测试命令 (cURL / SDK 示例)**。
     - **合规提示文案**：“出于安全合规要求，此密钥仅展示一次，关闭后将无法再次查看完整明文。如遗失可随时通过重置获取新密钥。”
   - 弹窗关闭后，前端状态必须彻底销毁该明文密钥，禁止在内存或全局状态中长期留存。

3. **历史只读脱敏 (Strict Masking for Saved Keys)**：
   - 列表（List）与详情（Detail）接口中，密钥字段必须脱敏（如 `sk-link-abc1••••••••ef89`）。
   - 禁止让前端复制脱敏省略号（`•••` 或 `...`），避免生成无效的死 Key 造成误导。
   - 在详情页中，若无当前操作产生的有效明文，复制与查看按钮必须给出明确的引导提示：“完整密钥仅在创建或重置时显示一次，如需新密钥请点击「重置密钥」”。

4. **遗失即重置 (Rotation Over Recovery)**：
   - 行业标准（AWS KMS, OpenAI, Anthropic, Stripe）不提供“反查已创建明文”的功能。
   - 密钥遗失的唯一合规补救机制是 **「重置密钥 (Rotate Key)」**：
     - 重置时使旧密钥立即作废（或在双密钥轮转宽限期内平滑过渡）；
     - 生成新密钥，并重新呼出合规的【重置成功模态弹窗】引导用户保存。

---

## 前端组件与交互规范 (Frontend UI Components)

### 1. 成功弹窗结构 (Create / Rotate Success Modal)
- **容器**：必须使用标准的模态弹窗（如 `@/components/ui/dialog`），设置 `max-w-2xl`，高度自适应且 `max-h-[85vh]`。
- **头部**：成功图标 + 标题（例如“密钥重置成功” / “API 密钥创建成功”）。
- **内容区**：
  - 密钥卡片：深色或灰底等宽高亮框，展示完整 Key。
  - 操作按钮组：
    - 「复制密钥」：复制成功后显示 Check 图标并 Toast 提示。
    - 「下载密钥」：触发下载形如 `api-key-<name>.txt` 的文本文件。
  - 接入示例：提供带有实际密钥的可测试 cURL 示例及参数说明。
  - 安全警示：明确标注一次性原则与不可逆哈希机制。
- **底部**：提供明确的「完成」或「关闭」按钮。

### 2. 详情卡片处理 (Saved Key Detail Card)
- 详情中的 Key 输入框必须明确呈现脱敏状态。
- 如果用户点击眼睛或复制，但该 Key 属于历史持久化 Key（无内存明文）：
  - 禁止复制带有 `•••` 的字符串；
  - 提示 Toast：“出于安全合规要求，完整密钥仅在创建或重置时可见一次；若已遗失，请点击重置密钥。”

---

## 检查清单 (PR / Review Checklist)

- [ ] 数据库没有新增任何存储明文 API Key 的字段。
- [ ] 密钥仅在 `create` 或 `rotate` API 响应体中返回 1 次，列表和查询 API 均返回脱敏哈希。
- [ ] 重置（Rotate）密钥后，使用标准 Modal 弹窗展示，而非页面横幅（banner）。
- [ ] 重置与创建成功弹窗均包含「复制」与「下载 .txt」按钮。
- [ ] 所有文案均遵循国际化 i18n 纯洁性要求（`zh.ts` 与 `en.ts` 对称写入，无混杂硬编码）。

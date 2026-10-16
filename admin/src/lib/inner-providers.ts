/**
 * Built-in ("inner") provider presets and their model catalogs.
 *
 * Reference data used for prefill and for the context / model-info fields; it is
 * not live gateway state. Entries were verified against the vendors' own docs on
 * 2026-10-08 (DeepSeek pricing/model table, Moonshot pricing, BigModel model
 * guides, MiniMax news/model pages). Fields that could not be verified stay
 * empty so the UI shows `—` instead of a made-up value — fill them here once
 * confirmed. No price data lives here on purpose.
 */
export type InnerModelKind = "chat" | "reasoner" | "code"

export interface InnerModel {
  /** Upstream model id, e.g. `deepseek-flash`. */
  id: string
  /** Context window as a display string (`1M`); empty when unverified. */
  context: string
  /** Capability label; localized through `models.kind*` keys. */
  kind: InnerModelKind
}

export interface InnerProvider {
  key: string
  /** Vendor display name; copied into the provider name field on prefill. */
  name: string
  /** OpenAI-compatible base URL; empty when unverified. */
  baseUrl: string
  /** Chat path override when the vendor has no `/v1/chat/completions`; empty keeps the default. */
  chatPath: string
  models: InnerModel[]
}

export const INNER_PROVIDERS: InnerProvider[] = [
  {
    key: "deepseek",
    name: "DeepSeek",
    baseUrl: "https://api.deepseek.com",
    chatPath: "",
    models: [
      { id: "deepseek-flash", context: "1M", kind: "chat" },
      { id: "deepseek-v4-pro", context: "1M", kind: "chat" },
    ],
  },
  {
    key: "kimi",
    name: "Kimi",
    baseUrl: "https://api.moonshot.cn/v1",
    chatPath: "",
    models: [
      { id: "kimi-k3", context: "1M", kind: "chat" },
      { id: "kimi-k2.6", context: "1M", kind: "chat" },
      { id: "kimi-k2.7-code", context: "1M", kind: "code" },
      { id: "kimi-k2.7-code-highspeed", context: "1M", kind: "code" },
    ],
  },
  {
    key: "zhipu",
    name: "Zhipu",
    baseUrl: "https://open.bigmodel.cn/api/paas/v4",
    chatPath: "/chat/completions",
    models: [
      { id: "glm-5.3", context: "1M", kind: "reasoner" },
      { id: "glm-5.3-flash", context: "", kind: "chat" },
      { id: "glm-5.2", context: "1M", kind: "chat" },
    ],
  },
  {
    key: "xiaomi",
    name: "Xiaomi",
    // Hosted API endpoint not published; MiMo ships as open weights.
    baseUrl: "",
    chatPath: "",
    models: [{ id: "mimo-7b-rl", context: "", kind: "reasoner" }],
  },
  {
    key: "minimax",
    name: "MiniMax",
    baseUrl: "https://api.minimax.chat/v1",
    chatPath: "",
    models: [
      { id: "MiniMax-M3", context: "", kind: "chat" },
      { id: "MiniMax-M2.5", context: "", kind: "code" },
      { id: "MiniMax-M2.7", context: "", kind: "code" },
      { id: "MiniMax-M1", context: "1M", kind: "reasoner" },
    ],
  },
]

function normalizeBaseUrl(url: string): string {
  return url.trim().replace(/\/+$/, "").toLowerCase()
}

/** Match a configured provider to a preset by base URL, then by name. */
export function findInnerProvider(
  provider: { name: string; base_url: string } | undefined,
): InnerProvider | undefined {
  if (!provider) return undefined
  const base = normalizeBaseUrl(provider.base_url)
  if (base) {
    const byUrl = INNER_PROVIDERS.find((item) => normalizeBaseUrl(item.baseUrl) === base)
    if (byUrl) return byUrl
  }
  return INNER_PROVIDERS.find((item) => item.name.toLowerCase() === provider.name.trim().toLowerCase())
}

/** Look up a catalog model for a provider + upstream model id. */
export function findInnerModel(
  provider: { name: string; base_url: string } | undefined,
  upstreamModel: string,
): InnerModel | undefined {
  const inner = findInnerProvider(provider)
  const id = upstreamModel.trim()
  return inner?.models.find((model) => model.id === id)
}

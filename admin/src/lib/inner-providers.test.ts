import { describe, expect, it } from "vitest"
import { INNER_PROVIDERS, findInnerModel, findInnerProvider, resolveInnerBaseUrl } from "./inner-providers"

describe("inner providers", () => {
  it("matches presets by base URL and Token Plan base URL regardless of trailing slash", () => {
    expect(findInnerProvider({ name: "whatever", base_url: "https://api.moonshot.cn/v1/" })?.key).toBe(
      "kimi",
    )
    expect(findInnerProvider({ name: "mi-payg", base_url: "https://api.xiaomimimo.com/v1/" })?.key).toBe(
      "xiaomi",
    )
    expect(
      findInnerProvider({ name: "mi-tp", base_url: "https://token-plan-cn.xiaomimimo.com/v1" })?.key,
    ).toBe("xiaomi")
  })

  it("resolves Token Plan base URL when a tp- or ttp- key is supplied", () => {
    const xiaomi = INNER_PROVIDERS.find((item) => item.key === "xiaomi")!
    expect(resolveInnerBaseUrl(xiaomi, "sk-regular")).toBe("https://api.xiaomimimo.com/v1")
    expect(resolveInnerBaseUrl(xiaomi, "tp-individual")).toBe("https://token-plan-cn.xiaomimimo.com/v1")
    expect(resolveInnerBaseUrl(xiaomi, "ttp-team")).toBe("https://token-plan-cn.xiaomimimo.com/v1")
  })

  it("falls back to the display name when the base URL is empty", () => {
    expect(findInnerProvider({ name: "Xiaomi", base_url: "" })?.key).toBe("xiaomi")
    expect(findInnerProvider({ name: "custom", base_url: "" })).toBeUndefined()
  })

  it("finds catalog models and reports unknown ones as undefined", () => {
    const provider = { name: "Kimi", base_url: "https://api.moonshot.cn/v1" }
    expect(findInnerModel(provider, "kimi-k2.6")?.context).toBe("1M")
    expect(
      findInnerModel(
        { name: "mi", base_url: "https://token-plan-cn.xiaomimimo.com/v1" },
        "mimo-v2.6-pro",
      )?.context,
    ).toBe("1M")
    expect(findInnerModel(provider, "nope")).toBeUndefined()
    expect(findInnerModel({ name: "custom", base_url: "" }, "deepseek-flash")).toBeUndefined()
  })
})

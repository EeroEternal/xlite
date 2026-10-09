import { describe, expect, it } from "vitest"
import { findInnerModel, findInnerProvider } from "./inner-providers"

describe("inner providers", () => {
  it("matches presets by base URL regardless of trailing slash", () => {
    expect(findInnerProvider({ name: "whatever", base_url: "https://api.moonshot.cn/v1/" })?.key).toBe(
      "kimi",
    )
  })

  it("falls back to the display name when the base URL is unverified", () => {
    expect(findInnerProvider({ name: "Xiaomi", base_url: "" })?.key).toBe("xiaomi")
    expect(findInnerProvider({ name: "custom", base_url: "" })).toBeUndefined()
  })

  it("finds catalog models and reports unknown ones as undefined", () => {
    const provider = { name: "Kimi", base_url: "https://api.moonshot.cn/v1" }
    expect(findInnerModel(provider, "kimi-k2.6")?.context).toBe("1M")
    expect(findInnerModel(provider, "nope")).toBeUndefined()
    expect(findInnerModel({ name: "custom", base_url: "" }, "deepseek-flash")).toBeUndefined()
  })
})

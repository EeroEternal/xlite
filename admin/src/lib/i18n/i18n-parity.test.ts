import { describe, expect, it } from "vitest"
import { en, zh } from "./locales"

type Dict = Record<string, unknown>

function resolveKey(dict: Dict, key: string): unknown {
  let value: unknown = dict
  for (const part of key.split(".")) {
    if (value && typeof value === "object" && part in (value as Dict)) {
      value = (value as Dict)[part]
    } else {
      return undefined
    }
  }
  return value
}

function flattenKeys(dict: Dict, prefix = ""): string[] {
  const keys: string[] = []
  for (const [k, v] of Object.entries(dict)) {
    const full = prefix ? `${prefix}.${k}` : k
    if (v && typeof v === "object") {
      keys.push(...flattenKeys(v as Dict, full))
    } else {
      keys.push(full)
    }
  }
  return keys
}

const sources = import.meta.glob("../../**/*.{ts,tsx}", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>

function collectTUsages(): Map<string, string> {
  const usages = new Map<string, string>()
  for (const [path, src] of Object.entries(sources)) {
    if (path.includes("/locales/") || path.endsWith(".test.ts") || path.endsWith(".test.tsx")) continue
    for (const match of src.matchAll(/\bt\(\s*["']([a-zA-Z0-9_.]+)["']/g)) {
      if (!usages.has(match[1])) {
        usages.set(match[1], path)
      }
    }
  }
  return usages
}

describe("i18n locale completeness", () => {
  it("every statically referenced t() key resolves to a string in both locales", () => {
    const usages = collectTUsages()
    expect(usages.size).toBeGreaterThan(10)

    const missing: string[] = []
    for (const [key, file] of usages) {
      if (typeof resolveKey(zh as unknown as Dict, key) !== "string") missing.push(`${key} (zh) <- ${file}`)
      if (typeof resolveKey(en as unknown as Dict, key) !== "string") missing.push(`${key} (en) <- ${file}`)
    }
    expect(missing.sort().join("\n"), "missing locale keys (add them to BOTH zh.ts and en.ts)").toBe("")
  })

  it("zh and en expose the exact same key set", () => {
    const zhKeys = new Set(flattenKeys(zh as unknown as Dict))
    const enKeys = new Set(flattenKeys(en as unknown as Dict))
    const zhOnly = [...zhKeys].filter((k) => !enKeys.has(k)).sort()
    const enOnly = [...enKeys].filter((k) => !enKeys.has(k)).sort()
    expect(`zh-only: ${zhOnly.join(", ")} | en-only: ${enOnly.join(", ")}`).toBe("zh-only:  | en-only: ")
  })
})

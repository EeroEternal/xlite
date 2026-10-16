import { describe, expect, it } from "vitest"
import { createProvider, deleteProvider, listProviders, updateProvider } from "./providers"

describe("provider resources", () => {
  it("keeps collection and item paths stable", () => {
    expect(listProviders.toString()).toContain("/api/providers")
    expect(createProvider.toString()).toContain("/api/providers")
    expect(updateProvider.toString()).toContain("/api/providers/")
    expect(deleteProvider.toString()).toContain("/api/providers/")
  })
})

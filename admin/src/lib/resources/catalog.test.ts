import { describe, expect, it } from "vitest"
import { exportCatalog, importCatalog } from "./catalog"

describe("catalog resources", () => {
  it("keeps import and export paths stable", () => {
    expect(exportCatalog.toString()).toContain("/api/catalog/export")
    expect(importCatalog.toString()).toContain("/api/catalog/import")
  })
})

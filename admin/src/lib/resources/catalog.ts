import { apiGet, apiPost } from "@/lib/api"
import type { ApiResponse } from "@/lib/api-types/common"
import type { CatalogYamlPayload, ImportReport } from "@/lib/api-types/catalog"

export function exportCatalog() {
  return apiGet<ApiResponse<CatalogYamlPayload>>("/api/catalog/export")
}

export function importCatalog(yaml: string, replace: boolean) {
  return apiPost<ApiResponse<ImportReport>>("/api/catalog/import", { yaml, replace })
}

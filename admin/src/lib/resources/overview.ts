import { apiGet } from "@/lib/api"
import type { ApiResponse } from "@/lib/api-types/common"
import type { Overview } from "@/lib/api-types/overview"

export function getOverview() {
  return apiGet<ApiResponse<Overview>>("/api/overview")
}

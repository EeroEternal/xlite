import { apiDelete, apiGet, apiPost, apiPut } from "@/lib/api"
import type { ApiResponse } from "@/lib/api-types/common"
import type { Provider, ProviderDraft } from "@/lib/api-types/providers"

export function listProviders() {
  return apiGet<ApiResponse<Provider[]>>("/api/providers")
}

export function createProvider(payload: ProviderDraft) {
  return apiPost<ApiResponse<Provider>>("/api/providers", payload)
}

export function updateProvider(id: number, payload: ProviderDraft) {
  return apiPut<ApiResponse<Provider>>(`/api/providers/${id}`, payload)
}

export function deleteProvider(id: number) {
  return apiDelete<ApiResponse<{ id: number }>>(`/api/providers/${id}`)
}

import { apiDelete, apiGet, apiPost, apiPut } from "@/lib/api"
import type { ApiKeyDraft, ApiKeyListItem, ApiKeySecretResult } from "@/lib/api-types/api-keys"
import type { ApiResponse } from "@/lib/api-types/common"

export function listApiKeys() {
  return apiGet<ApiResponse<ApiKeyListItem[]>>("/api/api-keys")
}

export function createApiKey(payload: ApiKeyDraft) {
  return apiPost<ApiResponse<ApiKeySecretResult>>("/api/api-keys", payload)
}

export function updateApiKey(id: number, payload: ApiKeyDraft) {
  return apiPut<ApiResponse<ApiKeyListItem>>(`/api/api-keys/${id}`, payload)
}

export function rotateApiKey(id: number) {
  return apiPost<ApiResponse<ApiKeySecretResult>>(`/api/api-keys/${id}/rotate`)
}

export function deleteApiKey(id: number) {
  return apiDelete<ApiResponse<{ id: number }>>(`/api/api-keys/${id}`)
}

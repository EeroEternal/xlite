import { apiDelete, apiGet, apiPost, apiPut } from "@/lib/api"
import type { ApiResponse } from "@/lib/api-types/common"
import type { Channel, ChannelDraft, Model, ModelDraft } from "@/lib/api-types/models"

export function listModels() {
  return apiGet<ApiResponse<Model[]>>("/api/models")
}

export function createModel(payload: ModelDraft) {
  return apiPost<ApiResponse<Model>>("/api/models", payload)
}

export function updateModel(id: number, payload: ModelDraft) {
  return apiPut<ApiResponse<Model>>(`/api/models/${id}`, payload)
}

export function deleteModel(id: number) {
  return apiDelete<ApiResponse<{ id: number }>>(`/api/models/${id}`)
}

export function listChannels() {
  return apiGet<ApiResponse<Channel[]>>("/api/channels")
}

export function createChannel(payload: ChannelDraft) {
  return apiPost<ApiResponse<Channel>>("/api/channels", payload)
}

export function updateChannel(id: number, payload: ChannelDraft) {
  return apiPut<ApiResponse<Channel>>(`/api/channels/${id}`, payload)
}

export function deleteChannel(id: number) {
  return apiDelete<ApiResponse<{ id: number }>>(`/api/channels/${id}`)
}

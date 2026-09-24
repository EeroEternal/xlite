import { apiGet, apiPost } from "@/lib/api"
import type { ApiResponse } from "@/lib/api-types/common"

export interface AuthUser {
  username: string
  role_id: number
}

export function login(username: string, password: string) {
  return apiPost<ApiResponse<{ user: AuthUser }>>("/api/auth/login", { username, password })
}

export function logout() {
  return apiPost<ApiResponse<{ ok: boolean }>>("/api/auth/logout")
}

export function getMe() {
  return apiGet<ApiResponse<AuthUser>>("/api/auth/me")
}

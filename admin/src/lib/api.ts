import { t } from "./i18n"

const getApiUrl = (): string => {
  if (import.meta.env.DEV) {
    return ""
  }
  return import.meta.env.VITE_API_URL || ""
}

export const API_URL = getApiUrl()

export async function apiCall<T>(endpoint: string, options?: RequestInit): Promise<T> {
  const url = `${API_URL}${endpoint}`
  try {
    const response = await fetch(url, {
      ...options,
      credentials: "include",
      headers: {
        "Content-Type": "application/json",
        ...options?.headers,
      },
    })

    if (response.status === 401) {
      if (!window.location.pathname.startsWith("/login")) {
        window.location.href = "/login"
      }
    }
    if (!response.ok) {
      const errData = await response.json().catch(() => null)
      let msg = errData?.message || errData?.error || `API error: ${response.status} ${response.statusText}`
      if (msg === "A record with this name or identifier already exists") {
        msg = t("common.alreadyExists")
      } else if (response.status === 403 && msg === "Forbidden") {
        msg = t("common.forbidden")
      }
      throw new Error(msg)
    }

    return await response.json()
  } catch (error) {
    console.error(`API call failed for ${endpoint}:`, error)
    throw error
  }
}

export function apiGet<T>(endpoint: string): Promise<T> {
  return apiCall<T>(endpoint, {
    method: "GET",
    cache: "no-store",
  })
}

export function apiPost<T>(endpoint: string, data?: unknown): Promise<T> {
  return apiCall<T>(endpoint, {
    method: "POST",
    body: data ? JSON.stringify(data) : undefined,
  })
}

export function apiPut<T>(endpoint: string, data?: unknown): Promise<T> {
  return apiCall<T>(endpoint, {
    method: "PUT",
    body: data ? JSON.stringify(data) : undefined,
  })
}

export function apiDelete<T>(endpoint: string): Promise<T> {
  return apiCall<T>(endpoint, { method: "DELETE" })
}

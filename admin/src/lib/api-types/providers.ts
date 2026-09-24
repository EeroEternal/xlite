export interface Provider {
  id: number
  name: string
  base_url: string
  enabled: boolean
  created_at: string
}

export interface ProviderDraft {
  name: string
  base_url: string
  api_key?: string
  enabled: boolean
}

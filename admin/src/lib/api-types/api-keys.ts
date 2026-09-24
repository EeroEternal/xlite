export interface ApiKeyListItem {
  id: number
  name: string
  key_prefix: string
  masked_key: string
  enabled: boolean
  created_at: string
  model_ids: number[]
}

export interface ApiKeySecretResult {
  id: number
  name: string
  key: string
  key_prefix: string
  masked_key: string
  enabled: boolean
  created_at: string
}

export interface ApiKeyDraft {
  name: string
  model_ids: number[]
  enabled: boolean
}

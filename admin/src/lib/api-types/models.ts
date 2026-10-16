export interface Model {
  id: number
  name: string
  enabled: boolean
  created_at: string
}

export interface Channel {
  id: number
  model_id: number
  provider_id: number
  upstream_model: string
  weight: number
  priority: number
  enabled: boolean
}

export interface ModelDraft {
  name: string
  enabled: boolean
}

export interface ChannelDraft {
  model_id: number
  provider_id: number
  upstream_model: string
  weight: number
  priority: number
  enabled: boolean
}

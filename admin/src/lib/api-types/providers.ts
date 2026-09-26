/** One static upstream request header. */
export interface ProviderHeader {
  name: string
  value: string
}

/**
 * Provider-level upstream request shape. The defaults reproduce xlite's original
 * `Bearer <key>` + `/v1/chat/completions` behaviour, so leaving these untouched
 * changes nothing about the upstream request.
 */
export interface ProviderUpstream {
  /** `bearer` | `raw` | `none` */
  auth_scheme: string
  /** Credential header name; empty means `authorization`. */
  auth_header: string
  extra_headers: ProviderHeader[]
  /** Chat path override; empty keeps the derived default. */
  chat_path: string
}

export interface Provider extends ProviderUpstream {
  id: number
  name: string
  base_url: string
  enabled: boolean
  created_at: string
}

export interface ProviderDraft extends ProviderUpstream {
  name: string
  base_url: string
  api_key?: string
  enabled: boolean
}

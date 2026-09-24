export interface ImportCounts {
  created: number
  updated: number
}

export interface CreatedSecret {
  name: string
  key: string
}

export interface ImportReport {
  providers: ImportCounts
  models: ImportCounts
  channels: ImportCounts
  api_keys: ImportCounts
  secrets: CreatedSecret[]
}

export interface CatalogYamlPayload {
  yaml: string
}

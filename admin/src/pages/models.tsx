import { useEffect, useMemo, useState } from "react"
import { toast } from "sonner"
import { EntityListToolbar } from "@/components/entity-list/EntityListToolbar"
import { PageContainer } from "@/components/layout/page-container"
import { PageHeader } from "@/components/layout/page-header"
import { PageShell } from "@/components/layout/page-shell"
import { ConfirmAlertDialog } from "@/components/ui/confirm-alert-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { t } from "@/lib/i18n"
import {
  INNER_PROVIDERS,
  findInnerModel,
  findInnerProvider,
} from "@/lib/inner-providers"
import type { Channel, Model } from "@/lib/api-types/models"
import type { Provider } from "@/lib/api-types/providers"
import {
  createChannel,
  createModel,
  deleteChannel,
  deleteModel,
  listChannels,
  listModels,
  updateChannel,
  updateModel,
} from "@/lib/resources/models"
import {
  createProvider,
  listProviders,
  updateProvider,
} from "@/lib/resources/providers"

type SortKey = "name-asc" | "name-desc"

/** One row per channel: a model bound to one provider upstream. */
interface ModelInstance {
  channel: Channel
  model: Model
  provider?: Provider
}

const CUSTOM_MODEL = "__custom__"
const CUSTOM_PROVIDER = "__custom__"

function kindLabel(kind: string): string {
  return t(`models.kind.${kind}`)
}

export function ModelsPage() {
  const [models, setModels] = useState<Model[]>([])
  const [channels, setChannels] = useState<Channel[]>([])
  const [providers, setProviders] = useState<Provider[]>([])
  const [search, setSearch] = useState("")
  const [sort, setSort] = useState<SortKey>("name-asc")
  const [open, setOpen] = useState(false)
  const [editing, setEditing] = useState<ModelInstance | null>(null)
  const [name, setName] = useState("")
  const [providerValue, setProviderValue] = useState("")
  const [upstreamModel, setUpstreamModel] = useState("")
  const [customModel, setCustomModel] = useState(false)
  const [apiKey, setApiKey] = useState("")
  const [customName, setCustomName] = useState("")
  const [customBaseUrl, setCustomBaseUrl] = useState("")
  const [weight, setWeight] = useState("1")
  const [priority, setPriority] = useState("0")
  const [enabled, setEnabled] = useState(true)
  const [pendingDelete, setPendingDelete] = useState<ModelInstance | null>(null)

  const load = async () => {
    const [modelRes, channelRes, providerRes] = await Promise.all([
      listModels(),
      listChannels(),
      listProviders(),
    ])
    setModels(modelRes.data ?? [])
    setChannels(channelRes.data ?? [])
    setProviders(providerRes.data ?? [])
  }

  useEffect(() => {
    let cancelled = false
    const run = async () => {
      try {
        const [modelRes, channelRes, providerRes] = await Promise.all([
          listModels(),
          listChannels(),
          listProviders(),
        ])
        if (cancelled) return
        setModels(modelRes.data ?? [])
        setChannels(channelRes.data ?? [])
        setProviders(providerRes.data ?? [])
      } catch (err) {
        if (!cancelled) toast.error((err as Error).message)
      }
    }
    void run()
    return () => {
      cancelled = true
    }
  }, [])

  const instances = useMemo<ModelInstance[]>(() => {
    const rows: ModelInstance[] = []
    for (const channel of channels) {
      const model = models.find((item) => item.id === channel.model_id)
      if (!model) continue
      rows.push({
        channel,
        model,
        provider: providers.find((item) => item.id === channel.provider_id),
      })
    }
    return rows
  }, [channels, models, providers])

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase()
    const next = instances.filter(
      (row) =>
        !q ||
        row.model.name.toLowerCase().includes(q) ||
        (row.provider?.name ?? "").toLowerCase().includes(q) ||
        row.channel.upstream_model.toLowerCase().includes(q),
    )
    next.sort((a, b) =>
      sort === "name-asc"
        ? a.model.name.localeCompare(b.model.name)
        : b.model.name.localeCompare(a.model.name),
    )
    return next
  }, [instances, search, sort])

  // Inner presets already backed by a configured provider are not listed twice.
  const innerOptions = INNER_PROVIDERS.filter(
    (preset) => !providers.some((item) => findInnerProvider(item)?.key === preset.key),
  )

  const providerOptions = [
    ...innerOptions.map((preset) => ({
      value: `inner:${preset.key}`,
      label: preset.name,
      group: t("providers.preset"),
    })),
    ...providers.map((item) => ({
      value: String(item.id),
      label: item.name,
      group: t("models.existingProviders"),
    })),
    { value: CUSTOM_PROVIDER, label: t("providers.presetCustom") },
  ]

  const selectedInner =
    providerValue.startsWith("inner:")
      ? INNER_PROVIDERS.find((item) => item.key === providerValue.slice(6))
      : undefined
  const selectedProvider = providers.find((item) => String(item.id) === providerValue)
  const catalogModels = (selectedInner ?? findInnerProvider(selectedProvider))?.models ?? []

  const openCreate = () => {
    setEditing(null)
    setName("")
    setProviderValue(providerOptions[0]?.value ?? "")
    setUpstreamModel("")
    setCustomModel(false)
    setApiKey("")
    setCustomName("")
    setCustomBaseUrl("")
    setWeight("1")
    setPriority("0")
    setEnabled(true)
    setOpen(true)
  }

  const openEdit = (row: ModelInstance) => {
    setEditing(row)
    setName(row.model.name)
    setProviderValue(String(row.channel.provider_id))
    setUpstreamModel(row.channel.upstream_model)
    setCustomModel(false)
    setApiKey("")
    setWeight(String(row.channel.weight))
    setPriority(String(row.channel.priority))
    setEnabled(row.channel.enabled && row.model.enabled)
    setOpen(true)
  }

  const catalogModel = findInnerModel(
    selectedInner
      ? { name: selectedInner.name, base_url: selectedInner.baseUrl }
      : selectedProvider,
    upstreamModel,
  )

  /** Resolve the provider for this instance: reuse a configured one or create it. */
  const resolveProvider = async (): Promise<number> => {
    if (providerValue === CUSTOM_PROVIDER) {
      const res = await createProvider({
        name: customName.trim(),
        base_url: customBaseUrl.trim(),
        api_key: apiKey,
        enabled: true,
        auth_scheme: "bearer",
        auth_header: "",
        extra_headers: [],
        chat_path: "",
      })
      if (!res.data) throw new Error(t("common.networkError"))
      return res.data.id
    }
    if (!selectedInner) return Number(providerValue)
    const existing = providers.find((item) => findInnerProvider(item)?.key === selectedInner.key)
    if (existing) {
      if (apiKey) {
        await updateProvider(existing.id, {
          name: existing.name,
          base_url: existing.base_url,
          api_key: apiKey,
          enabled: existing.enabled,
          auth_scheme: existing.auth_scheme,
          auth_header: existing.auth_header,
          extra_headers: existing.extra_headers,
          chat_path: existing.chat_path,
        })
      }
      return existing.id
    }
    const res = await createProvider({
      name: selectedInner.name,
      base_url: selectedInner.baseUrl,
      api_key: apiKey,
      enabled: true,
      auth_scheme: "bearer",
      auth_header: "",
      extra_headers: [],
      chat_path: selectedInner.chatPath,
    })
    if (!res.data) throw new Error(t("common.networkError"))
    return res.data.id
  }

  const handleSave = async () => {
    try {
      const trimmed = name.trim()
      const providerId = await resolveProvider()
      const payload = {
        provider_id: providerId,
        upstream_model: upstreamModel.trim(),
        weight: Number(weight) || 1,
        priority: Number(priority) || 0,
        enabled,
      }
      if (editing) {
        if (trimmed !== editing.model.name) {
          await updateModel(editing.model.id, { name: trimmed, enabled: editing.model.enabled })
        }
        await updateChannel(editing.channel.id, { model_id: editing.model.id, ...payload })
      } else {
        let model = models.find((item) => item.name === trimmed)
        if (!model) {
          const res = await createModel({ name: trimmed, enabled: true })
          if (!res.data) throw new Error(t("common.networkError"))
          model = res.data
        }
        await createChannel({ model_id: model.id, ...payload })
      }
      setOpen(false)
      await load()
    } catch (err) {
      toast.error((err as Error).message)
    }
  }

  const handleDelete = async (row: ModelInstance) => {
    await deleteChannel(row.channel.id)
    const remaining = channels.filter(
      (item) => item.model_id === row.model.id && item.id !== row.channel.id,
    )
    if (remaining.length === 0) await deleteModel(row.model.id)
  }

  return (
    <PageShell>
      <PageContainer>
        <PageHeader
          title={t("models.title")}
          action={<Button onClick={openCreate}>{t("models.create")}</Button>}
        />
        <EntityListToolbar
          searchValue={search}
          onSearchChange={setSearch}
          searchPlaceholder={t("models.searchPlaceholder")}
          sort={
            <Select
              value={sort}
              onChange={(value) => setSort(value as SortKey)}
              options={[
                { value: "name-asc", label: t("models.sortNameAsc") },
                { value: "name-desc", label: t("models.sortNameDesc") },
              ]}
            />
          }
          resultCount={t("common.total").replace("{n}", String(filtered.length))}
        />
        <Card className="overflow-hidden py-0">
          {filtered.length === 0 ? (
            <p className="px-6 py-12 text-center text-sm text-muted-foreground">{t("models.empty")}</p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t("models.name")}</TableHead>
                  <TableHead>{t("models.provider")}</TableHead>
                  <TableHead>{t("models.upstreamModel")}</TableHead>
                  <TableHead>{t("models.context")}</TableHead>
                  <TableHead>{t("models.modelInfo")}</TableHead>
                  <TableHead>{t("common.status")}</TableHead>
                  <TableHead className="text-right">{t("common.actions")}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {filtered.map((row) => {
                  const hit = findInnerModel(row.provider, row.channel.upstream_model)
                  return (
                    <TableRow key={row.channel.id}>
                      <TableCell className="font-medium">{row.model.name}</TableCell>
                      <TableCell>{row.provider?.name ?? String(row.channel.provider_id)}</TableCell>
                      <TableCell className="font-mono text-xs">{row.channel.upstream_model}</TableCell>
                      <TableCell>{hit?.context || "—"}</TableCell>
                      <TableCell>{hit ? kindLabel(hit.kind) : "—"}</TableCell>
                      <TableCell>
                        <Badge
                          variant={
                            row.channel.enabled && row.model.enabled ? "success" : "inactive"
                          }
                        >
                          {row.channel.enabled && row.model.enabled
                            ? t("common.enabled")
                            : t("common.disabled")}
                        </Badge>
                      </TableCell>
                      <TableCell className="text-right">
                        <Button variant="ghost" size="sm" onClick={() => openEdit(row)}>
                          {t("common.edit")}
                        </Button>
                        <Button variant="ghost" size="sm" onClick={() => setPendingDelete(row)}>
                          {t("common.delete")}
                        </Button>
                      </TableCell>
                    </TableRow>
                  )
                })}
              </TableBody>
            </Table>
          )}
        </Card>

        <Dialog open={open} onOpenChange={setOpen}>
          <DialogContent className="flex max-h-[85vh] w-full max-w-lg flex-col overflow-hidden">
            <DialogHeader>
              <DialogTitle>{editing ? t("models.edit") : t("models.create")}</DialogTitle>
            </DialogHeader>
            <div className="min-h-0 flex-1 -mx-1 space-y-3 overflow-y-auto px-1">
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label htmlFor="model-name">{t("models.name")}</Label>
                  <Input
                    id="model-name"
                    value={name}
                    placeholder={t("models.nameHint")}
                    onChange={(e) => setName(e.target.value)}
                  />
                </div>
                <div className="space-y-2">
                  <Label>{t("models.provider")}</Label>
                  <Select
                    triggerClassName="h-10 rounded-sm"
                    value={providerValue}
                    onChange={(value) => {
                      setProviderValue(value)
                      setCustomModel(false)
                      setUpstreamModel("")
                    }}
                    options={providerOptions}
                  />
                </div>
              </div>
              {/* Reserved slot: swapping provider detail must not shove the dialog. */}
              <div className="flex min-h-10 items-center gap-3">
                {providerValue === CUSTOM_PROVIDER ? (
                  <div className="grid w-full grid-cols-2 gap-3">
                    <Input
                      aria-label={t("providers.name")}
                      placeholder={t("providers.name")}
                      value={customName}
                      onChange={(e) => setCustomName(e.target.value)}
                    />
                    <Input
                      aria-label={t("providers.baseUrl")}
                      placeholder={t("providers.baseUrl")}
                      value={customBaseUrl}
                      onChange={(e) => setCustomBaseUrl(e.target.value)}
                    />
                  </div>
                ) : (
                  <div className="flex w-full items-baseline gap-2 rounded-lg bg-muted/30 px-3 py-1.5">
                    <span className="text-label-sm text-muted-foreground">{t("providers.baseUrl")}</span>
                    <span className="truncate font-mono text-sm text-foreground">
                      {selectedInner?.baseUrl || selectedProvider?.base_url || "—"}
                    </span>
                  </div>
                )}
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label>{t("models.upstreamModel")}</Label>
                  {catalogModels.length > 0 ? (
                    <>
                      <Select
                        triggerClassName="h-10 rounded-sm"
                        value={customModel ? CUSTOM_MODEL : upstreamModel}
                        onChange={(value) => {
                          if (value === CUSTOM_MODEL) {
                            setCustomModel(true)
                            setUpstreamModel("")
                          } else {
                            setCustomModel(false)
                            setUpstreamModel(value)
                          }
                        }}
                        options={[
                          ...catalogModels.map((model) => ({ value: model.id, label: model.id })),
                          { value: CUSTOM_MODEL, label: t("models.customModel") },
                        ]}
                      />
                      {/* Reserved slot: custom input or catalog facts, never a blank gap. */}
                      <div className="flex min-h-10 items-center rounded-lg bg-muted/30 px-3">
                        {customModel ? (
                          <Input
                            id="upstream-model"
                            value={upstreamModel}
                            placeholder={t("models.customModel")}
                            onChange={(e) => setUpstreamModel(e.target.value)}
                          />
                        ) : (
                          <div className="flex items-baseline gap-4">
                            <div className="flex items-baseline gap-1.5">
                              <span className="text-label-sm text-muted-foreground">{t("models.context")}</span>
                              <span className="text-sm text-foreground">{catalogModel?.context || "—"}</span>
                            </div>
                            <div className="flex items-baseline gap-1.5">
                              <span className="text-label-sm text-muted-foreground">{t("models.modelInfo")}</span>
                              <span className="text-sm text-foreground">
                                {catalogModel ? kindLabel(catalogModel.kind) : "—"}
                              </span>
                            </div>
                          </div>
                        )}
                      </div>
                    </>
                  ) : (
                    <Input
                      id="upstream-model"
                      value={upstreamModel}
                      onChange={(e) => setUpstreamModel(e.target.value)}
                    />
                  )}
                </div>
                <div className="space-y-2">
                  <Label htmlFor="instance-api-key">{t("providers.apiKey")}</Label>
                  <Input
                    id="instance-api-key"
                    type="password"
                    value={apiKey}
                    placeholder={t("providers.apiKeyHint")}
                    onChange={(e) => setApiKey(e.target.value)}
                  />
                </div>
              </div>
              {editing ? (
                <>
                  <div className="grid grid-cols-2 gap-3">
                    <div className="space-y-2">
                      <Label htmlFor="weight">{t("models.weight")}</Label>
                      <Input id="weight" value={weight} placeholder="1" onChange={(e) => setWeight(e.target.value)} />
                    </div>
                    <div className="space-y-2">
                      <Label htmlFor="priority">{t("models.priority")}</Label>
                      <Input id="priority" value={priority} placeholder="0" onChange={(e) => setPriority(e.target.value)} />
                    </div>
                  </div>
                  <p className="text-meta-sm text-muted-foreground">
                    {t("models.weightHint")} · {t("models.priorityHint")}
                  </p>
                  <div className="flex items-center justify-between rounded-lg bg-muted/30 px-3 py-2">
                    <Label htmlFor="instance-enabled">{t("common.enabled")}</Label>
                    <Switch id="instance-enabled" checked={enabled} onCheckedChange={setEnabled} />
                  </div>
                </>
              ) : null}
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setOpen(false)}>
                {t("common.cancel")}
              </Button>
              <Button onClick={() => void handleSave()}>{t("common.save")}</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        <ConfirmAlertDialog
          open={Boolean(pendingDelete)}
          onOpenChange={(next) => {
            if (!next) setPendingDelete(null)
          }}
          title={t("models.deleteTitle")}
          description={t("models.deleteDescription")}
          cancelText={t("common.cancel")}
          confirmText={t("common.delete")}
          confirmClassName="bg-destructive text-destructive-foreground"
          onConfirm={() => {
            if (!pendingDelete) return
            void handleDelete(pendingDelete)
              .then(() => load())
              .catch((err: Error) => toast.error(err.message))
            setPendingDelete(null)
          }}
        />
      </PageContainer>
    </PageShell>
  )
}

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
import { listProviders } from "@/lib/resources/providers"

type SortKey = "name-asc" | "name-desc"

export function ModelsPage() {
  const [models, setModels] = useState<Model[]>([])
  const [channels, setChannels] = useState<Channel[]>([])
  const [providers, setProviders] = useState<Provider[]>([])
  const [search, setSearch] = useState("")
  const [sort, setSort] = useState<SortKey>("name-asc")
  const [modelOpen, setModelOpen] = useState(false)
  const [editingModel, setEditingModel] = useState<Model | null>(null)
  const [modelName, setModelName] = useState("")
  const [modelEnabled, setModelEnabled] = useState(true)
  const [detail, setDetail] = useState<Model | null>(null)
  const [channelOpen, setChannelOpen] = useState(false)
  const [editingChannel, setEditingChannel] = useState<Channel | null>(null)
  const [providerId, setProviderId] = useState("")
  const [upstreamModel, setUpstreamModel] = useState("")
  const [weight, setWeight] = useState("1")
  const [priority, setPriority] = useState("0")
  const [channelEnabled, setChannelEnabled] = useState(true)
  const [pendingModel, setPendingModel] = useState<Model | null>(null)
  const [pendingChannel, setPendingChannel] = useState<Channel | null>(null)

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

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase()
    const next = models.filter((item) => !q || item.name.toLowerCase().includes(q))
    next.sort((a, b) =>
      sort === "name-asc" ? a.name.localeCompare(b.name) : b.name.localeCompare(a.name),
    )
    return next
  }, [models, search, sort])

  const detailChannels = channels.filter((channel) => channel.model_id === detail?.id)
  const providerName = (id: number) => providers.find((item) => item.id === id)?.name ?? String(id)

  const openCreateModel = () => {
    setEditingModel(null)
    setModelName("")
    setModelEnabled(true)
    setModelOpen(true)
  }

  const openEditModel = (item: Model) => {
    setEditingModel(item)
    setModelName(item.name)
    setModelEnabled(item.enabled)
    setModelOpen(true)
  }

  const saveModel = async () => {
    try {
      const payload = { name: modelName, enabled: modelEnabled }
      if (editingModel) await updateModel(editingModel.id, payload)
      else await createModel(payload)
      setModelOpen(false)
      await load()
    } catch (err) {
      toast.error((err as Error).message)
    }
  }

  const openCreateChannel = () => {
    setEditingChannel(null)
    setProviderId(providers[0] ? String(providers[0].id) : "")
    setUpstreamModel(detail?.name ?? "")
    setWeight("1")
    setPriority("0")
    setChannelEnabled(true)
    setChannelOpen(true)
  }

  const openEditChannel = (channel: Channel) => {
    setEditingChannel(channel)
    setProviderId(String(channel.provider_id))
    setUpstreamModel(channel.upstream_model)
    setWeight(String(channel.weight))
    setPriority(String(channel.priority))
    setChannelEnabled(channel.enabled)
    setChannelOpen(true)
  }

  const saveChannel = async () => {
    if (!detail) return
    try {
      const payload = {
        model_id: detail.id,
        provider_id: Number(providerId),
        upstream_model: upstreamModel,
        weight: Number(weight) || 1,
        priority: Number(priority) || 0,
        enabled: channelEnabled,
      }
      if (editingChannel) await updateChannel(editingChannel.id, payload)
      else await createChannel(payload)
      setChannelOpen(false)
      await load()
    } catch (err) {
      toast.error((err as Error).message)
    }
  }

  return (
    <PageShell>
      <PageContainer>
        <PageHeader
          title={t("models.title")}
          action={<Button onClick={openCreateModel}>{t("models.create")}</Button>}
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
                  <TableHead>{t("models.channels")}</TableHead>
                  <TableHead>{t("common.status")}</TableHead>
                  <TableHead className="text-right">{t("common.actions")}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {filtered.map((item) => {
                  const count = channels.filter((channel) => channel.model_id === item.id).length
                  return (
                    <TableRow key={item.id}>
                      <TableCell className="font-medium">{item.name}</TableCell>
                      <TableCell>{t("models.channelCount").replace("{n}", String(count))}</TableCell>
                      <TableCell>
                        <Badge variant={item.enabled ? "success" : "inactive"}>
                          {item.enabled ? t("common.enabled") : t("common.disabled")}
                        </Badge>
                      </TableCell>
                      <TableCell className="text-right">
                        <Button variant="ghost" size="sm" onClick={() => setDetail(item)}>
                          {t("models.channels")}
                        </Button>
                        <Button variant="ghost" size="sm" onClick={() => openEditModel(item)}>
                          {t("common.edit")}
                        </Button>
                        <Button variant="ghost" size="sm" onClick={() => setPendingModel(item)}>
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

        <Dialog open={modelOpen} onOpenChange={setModelOpen}>
          <DialogContent className="w-full max-w-lg">
            <DialogHeader>
              <DialogTitle>{editingModel ? t("models.edit") : t("models.create")}</DialogTitle>
            </DialogHeader>
            <div className="space-y-4">
              <div className="space-y-2">
                <Label htmlFor="model-name">{t("models.name")}</Label>
                <Input id="model-name" value={modelName} onChange={(e) => setModelName(e.target.value)} />
                <p className="text-meta-sm text-muted-foreground">{t("models.nameHint")}</p>
              </div>
              <div className="flex items-center justify-between rounded-lg bg-muted/30 px-3 py-2">
                <Label htmlFor="model-enabled">{t("common.enabled")}</Label>
                <Switch id="model-enabled" checked={modelEnabled} onCheckedChange={setModelEnabled} />
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setModelOpen(false)}>
                {t("common.cancel")}
              </Button>
              <Button onClick={() => void saveModel()}>{t("common.save")}</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        <Dialog open={Boolean(detail)} onOpenChange={(next) => !next && setDetail(null)}>
          <DialogContent className="flex max-h-[85vh] w-full max-w-3xl flex-col overflow-hidden">
            <DialogHeader>
              <DialogTitle>
                {detail?.name} · {t("models.channels")}
              </DialogTitle>
            </DialogHeader>
            <div className="min-h-0 flex-1 overflow-y-auto">
              <div className="mb-3 flex justify-end">
                <Button size="sm" onClick={openCreateChannel}>
                  {t("models.addChannel")}
                </Button>
              </div>
              {detailChannels.length === 0 ? (
                <p className="py-8 text-center text-sm text-muted-foreground">{t("models.emptyChannels")}</p>
              ) : (
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>{t("models.provider")}</TableHead>
                      <TableHead>{t("models.upstreamModel")}</TableHead>
                      <TableHead>{t("models.weight")}</TableHead>
                      <TableHead>{t("models.priority")}</TableHead>
                      <TableHead>{t("common.status")}</TableHead>
                      <TableHead className="text-right">{t("common.actions")}</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {detailChannels.map((channel) => (
                      <TableRow key={channel.id}>
                        <TableCell>{providerName(channel.provider_id)}</TableCell>
                        <TableCell className="font-mono text-xs">{channel.upstream_model}</TableCell>
                        <TableCell>{channel.weight}</TableCell>
                        <TableCell>{channel.priority}</TableCell>
                        <TableCell>
                          <Badge variant={channel.enabled ? "success" : "inactive"}>
                            {channel.enabled ? t("common.enabled") : t("common.disabled")}
                          </Badge>
                        </TableCell>
                        <TableCell className="text-right">
                          <Button variant="ghost" size="sm" onClick={() => openEditChannel(channel)}>
                            {t("common.edit")}
                          </Button>
                          <Button variant="ghost" size="sm" onClick={() => setPendingChannel(channel)}>
                            {t("common.delete")}
                          </Button>
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              )}
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setDetail(null)}>
                {t("common.done")}
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        <Dialog open={channelOpen} onOpenChange={setChannelOpen}>
          <DialogContent className="w-full max-w-lg">
            <DialogHeader>
              <DialogTitle>{editingChannel ? t("models.editChannel") : t("models.addChannel")}</DialogTitle>
            </DialogHeader>
            <div className="space-y-4">
              <div className="space-y-2">
                <Label>{t("models.provider")}</Label>
                <Select
                  value={providerId}
                  onChange={setProviderId}
                  options={providers.map((item) => ({ value: String(item.id), label: item.name }))}
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="upstream-model">{t("models.upstreamModel")}</Label>
                <Input
                  id="upstream-model"
                  value={upstreamModel}
                  onChange={(e) => setUpstreamModel(e.target.value)}
                />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label htmlFor="weight">{t("models.weight")}</Label>
                  <Input id="weight" value={weight} onChange={(e) => setWeight(e.target.value)} />
                  <p className="text-meta-sm text-muted-foreground">{t("models.weightHint")}</p>
                </div>
                <div className="space-y-2">
                  <Label htmlFor="priority">{t("models.priority")}</Label>
                  <Input id="priority" value={priority} onChange={(e) => setPriority(e.target.value)} />
                  <p className="text-meta-sm text-muted-foreground">{t("models.priorityHint")}</p>
                </div>
              </div>
              <div className="flex items-center justify-between rounded-lg bg-muted/30 px-3 py-2">
                <Label htmlFor="channel-enabled">{t("common.enabled")}</Label>
                <Switch id="channel-enabled" checked={channelEnabled} onCheckedChange={setChannelEnabled} />
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setChannelOpen(false)}>
                {t("common.cancel")}
              </Button>
              <Button onClick={() => void saveChannel()}>{t("common.save")}</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        <ConfirmAlertDialog
          open={Boolean(pendingModel)}
          onOpenChange={(next) => {
            if (!next) setPendingModel(null)
          }}
          title={t("models.deleteTitle")}
          description={t("models.deleteDescription")}
          cancelText={t("common.cancel")}
          confirmText={t("common.delete")}
          confirmClassName="bg-destructive text-destructive-foreground"
          onConfirm={() => {
            if (!pendingModel) return
            void deleteModel(pendingModel.id)
              .then(() => load())
              .catch((err: Error) => toast.error(err.message))
            setPendingModel(null)
          }}
        />
        <ConfirmAlertDialog
          open={Boolean(pendingChannel)}
          onOpenChange={(next) => {
            if (!next) setPendingChannel(null)
          }}
          title={t("models.deleteChannelTitle")}
          description={t("models.deleteChannelDescription")}
          cancelText={t("common.cancel")}
          confirmText={t("common.delete")}
          confirmClassName="bg-destructive text-destructive-foreground"
          onConfirm={() => {
            if (!pendingChannel) return
            void deleteChannel(pendingChannel.id)
              .then(() => load())
              .catch((err: Error) => toast.error(err.message))
            setPendingChannel(null)
          }}
        />
      </PageContainer>
    </PageShell>
  )
}

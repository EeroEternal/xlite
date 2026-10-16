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
import { MultiSelect } from "@/components/ui/multi-select"
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
import { copyTextToClipboard } from "@/lib/utils"
import { t } from "@/lib/i18n"
import type { ApiKeyListItem, ApiKeySecretResult } from "@/lib/api-types/api-keys"
import type { Model } from "@/lib/api-types/models"
import {
  createApiKey,
  deleteApiKey,
  listApiKeys,
  rotateApiKey,
  updateApiKey,
} from "@/lib/resources/api-keys"
import { listModels } from "@/lib/resources/models"

type SortKey = "name-asc" | "name-desc"

export function ApiKeysPage() {
  const [items, setItems] = useState<ApiKeyListItem[]>([])
  const [models, setModels] = useState<Model[]>([])
  const [search, setSearch] = useState("")
  const [sort, setSort] = useState<SortKey>("name-asc")
  const [open, setOpen] = useState(false)
  const [editing, setEditing] = useState<ApiKeyListItem | null>(null)
  const [name, setName] = useState("")
  const [modelIds, setModelIds] = useState<number[]>([])
  const [enabled, setEnabled] = useState(true)
  const [secret, setSecret] = useState<ApiKeySecretResult | null>(null)
  const [secretTitle, setSecretTitle] = useState("")
  const [pendingDelete, setPendingDelete] = useState<ApiKeyListItem | null>(null)
  const [pendingRotate, setPendingRotate] = useState<ApiKeyListItem | null>(null)

  const load = async () => {
    const [keyRes, modelRes] = await Promise.all([listApiKeys(), listModels()])
    setItems(keyRes.data ?? [])
    setModels(modelRes.data ?? [])
  }

  useEffect(() => {
    let cancelled = false
    const run = async () => {
      try {
        const [keyRes, modelRes] = await Promise.all([listApiKeys(), listModels()])
        if (cancelled) return
        setItems(keyRes.data ?? [])
        setModels(modelRes.data ?? [])
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
    const next = items.filter(
      (item) =>
        !q ||
        item.name.toLowerCase().includes(q) ||
        item.key_prefix.toLowerCase().includes(q),
    )
    next.sort((a, b) =>
      sort === "name-asc" ? a.name.localeCompare(b.name) : b.name.localeCompare(a.name),
    )
    return next
  }, [items, search, sort])

  const openCreate = () => {
    setEditing(null)
    setName("")
    setModelIds([])
    setEnabled(true)
    setOpen(true)
  }

  const openEdit = (item: ApiKeyListItem) => {
    setEditing(item)
    setName(item.name)
    setModelIds(item.model_ids ?? [])
    setEnabled(item.enabled)
    setOpen(true)
  }

  const handleSave = async () => {
    try {
      const payload = { name, model_ids: modelIds, enabled }
      if (editing) {
        await updateApiKey(editing.id, payload)
        setOpen(false)
      } else {
        const res = await createApiKey(payload)
        setOpen(false)
        if (res.data?.key) {
          setSecretTitle(t("keys.createdTitle"))
          setSecret(res.data)
        }
      }
      await load()
    } catch (err) {
      toast.error((err as Error).message)
    }
  }

  const handleRotate = async (item: ApiKeyListItem) => {
    try {
      const res = await rotateApiKey(item.id)
      if (res.data?.key) {
        setSecretTitle(t("keys.rotatedTitle"))
        setSecret(res.data)
      }
      await load()
    } catch (err) {
      toast.error((err as Error).message)
    }
  }

  const downloadKey = (value: ApiKeySecretResult) => {
    const blob = new Blob([value.key], { type: "text/plain" })
    const url = URL.createObjectURL(blob)
    const link = document.createElement("a")
    link.href = url
    link.download = `api-key-${value.name}.txt`
    link.click()
    URL.revokeObjectURL(url)
  }

  const example = secret ? `curl -H "Authorization: Bearer ${secret.key}" http://127.0.0.1:8080/v1/models` : ""

  return (
    <PageShell>
      <PageContainer>
        <PageHeader
          title={t("keys.title")}
          action={<Button onClick={openCreate}>{t("keys.create")}</Button>}
        />
        <EntityListToolbar
          searchValue={search}
          onSearchChange={setSearch}
          searchPlaceholder={t("keys.searchPlaceholder")}
          sort={
            <Select
              value={sort}
              onChange={(value) => setSort(value as SortKey)}
              options={[
                { value: "name-asc", label: t("keys.sortNameAsc") },
                { value: "name-desc", label: t("keys.sortNameDesc") },
              ]}
            />
          }
          resultCount={t("common.total").replace("{n}", String(filtered.length))}
        />
        <Card className="overflow-hidden py-0">
          {filtered.length === 0 ? (
            <p className="px-6 py-12 text-center text-sm text-muted-foreground">{t("keys.empty")}</p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t("keys.name")}</TableHead>
                  <TableHead>{t("keys.key")}</TableHead>
                  <TableHead>{t("keys.models")}</TableHead>
                  <TableHead>{t("common.status")}</TableHead>
                  <TableHead className="text-right">{t("common.actions")}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {filtered.map((item) => (
                  <TableRow key={item.id}>
                    <TableCell className="font-medium">{item.name}</TableCell>
                    <TableCell className="font-mono text-xs">{item.masked_key}</TableCell>
                    <TableCell>
                      {(item.model_ids ?? []).length === 0
                        ? t("keys.modelsAll")
                        : (item.model_ids ?? [])
                            .map((id) => models.find((model) => model.id === id)?.name ?? String(id))
                            .join(", ")}
                    </TableCell>
                    <TableCell>
                      <Badge variant={item.enabled ? "success" : "inactive"}>
                        {item.enabled ? t("common.enabled") : t("common.disabled")}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-right">
                      <Button variant="ghost" size="sm" onClick={() => openEdit(item)}>
                        {t("common.edit")}
                      </Button>
                      <Button variant="ghost" size="sm" onClick={() => setPendingRotate(item)}>
                        {t("keys.rotate")}
                      </Button>
                      <Button variant="ghost" size="sm" onClick={() => setPendingDelete(item)}>
                        {t("common.delete")}
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </Card>

        <Dialog open={open} onOpenChange={setOpen}>
          <DialogContent className="w-full max-w-lg">
            <DialogHeader>
              <DialogTitle>{editing ? t("keys.edit") : t("keys.create")}</DialogTitle>
            </DialogHeader>
            <div className="space-y-4">
              <div className="space-y-2">
                <Label htmlFor="key-name">{t("keys.name")}</Label>
                <Input id="key-name" value={name} onChange={(e) => setName(e.target.value)} />
              </div>
              <div className="space-y-2">
                <Label>{t("keys.models")}</Label>
                <MultiSelect
                  options={models.map((model) => ({ value: model.id, label: model.name }))}
                  value={modelIds}
                  onChange={setModelIds}
                  placeholder={t("keys.modelsHint")}
                />
                <p className="text-meta-sm text-muted-foreground">{t("keys.modelsHint")}</p>
              </div>
              {editing ? (
                <p className="text-meta-sm text-muted-foreground">{t("keys.maskedHint")}</p>
              ) : null}
              <div className="flex items-center justify-between rounded-lg bg-muted/30 px-3 py-2">
                <Label htmlFor="key-enabled">{t("common.enabled")}</Label>
                <Switch id="key-enabled" checked={enabled} onCheckedChange={setEnabled} />
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setOpen(false)}>
                {t("common.cancel")}
              </Button>
              <Button onClick={() => void handleSave()}>{t("common.save")}</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        <Dialog
          open={Boolean(secret)}
          onOpenChange={(next) => {
            if (!next) setSecret(null)
          }}
        >
          <DialogContent className="w-full max-w-2xl">
            <DialogHeader>
              <DialogTitle>{secretTitle}</DialogTitle>
            </DialogHeader>
            {secret ? (
              <div className="space-y-4">
                <div className="rounded-lg bg-muted/30 p-3 font-mono text-sm break-all">{secret.key}</div>
                <p className="text-sm text-muted-foreground">{t("keys.onceHint")}</p>
                <div className="flex flex-wrap gap-2">
                  <Button
                    variant="outline"
                    onClick={() => {
                      void copyTextToClipboard(secret.key).then(() => toast.success(t("common.copied")))
                    }}
                  >
                    {t("common.copy")}
                  </Button>
                  <Button variant="outline" onClick={() => downloadKey(secret)}>
                    {t("common.download")}
                  </Button>
                  <Button
                    variant="outline"
                    onClick={() => {
                      void copyTextToClipboard(example).then(() => toast.success(t("common.copied")))
                    }}
                  >
                    {t("keys.copyCommand")}
                  </Button>
                </div>
              </div>
            ) : null}
            <DialogFooter>
              <Button onClick={() => setSecret(null)}>{t("common.done")}</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        <ConfirmAlertDialog
          open={Boolean(pendingDelete)}
          onOpenChange={(next) => {
            if (!next) setPendingDelete(null)
          }}
          title={t("keys.deleteTitle")}
          description={t("keys.deleteDescription")}
          cancelText={t("common.cancel")}
          confirmText={t("common.delete")}
          confirmClassName="bg-destructive text-destructive-foreground"
          onConfirm={() => {
            if (!pendingDelete) return
            void deleteApiKey(pendingDelete.id)
              .then(() => load())
              .catch((err: Error) => toast.error(err.message))
            setPendingDelete(null)
          }}
        />
        <ConfirmAlertDialog
          open={Boolean(pendingRotate)}
          onOpenChange={(next) => {
            if (!next) setPendingRotate(null)
          }}
          title={t("keys.rotateTitle")}
          description={t("keys.rotateDescription")}
          cancelText={t("common.cancel")}
          confirmText={t("keys.rotate")}
          onConfirm={() => {
            if (!pendingRotate) return
            void handleRotate(pendingRotate)
            setPendingRotate(null)
          }}
        />
      </PageContainer>
    </PageShell>
  )
}

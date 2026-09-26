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
import type { Provider, ProviderHeader } from "@/lib/api-types/providers"
import {
  createProvider,
  deleteProvider,
  listProviders,
  updateProvider,
} from "@/lib/resources/providers"

type SortKey = "name-asc" | "name-desc"

/** `Name: value` per line, matching the hint shown under the field. */
function parseHeaders(text: string): ProviderHeader[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const at = line.indexOf(":")
      if (at < 0) return { name: line, value: "" }
      return { name: line.slice(0, at).trim(), value: line.slice(at + 1).trim() }
    })
    .filter((header) => header.name.length > 0)
}

function formatHeaders(headers: ProviderHeader[] | undefined): string {
  return (headers ?? []).map((header) => `${header.name}: ${header.value}`).join("\n")
}

export function ProvidersPage() {
  const [items, setItems] = useState<Provider[]>([])
  const [search, setSearch] = useState("")
  const [sort, setSort] = useState<SortKey>("name-asc")
  const [open, setOpen] = useState(false)
  const [editing, setEditing] = useState<Provider | null>(null)
  const [name, setName] = useState("")
  const [baseUrl, setBaseUrl] = useState("")
  const [apiKey, setApiKey] = useState("")
  const [enabled, setEnabled] = useState(true)
  const [authScheme, setAuthScheme] = useState("bearer")
  const [authHeader, setAuthHeader] = useState("")
  const [headersText, setHeadersText] = useState("")
  const [chatPath, setChatPath] = useState("")
  const [pendingDelete, setPendingDelete] = useState<Provider | null>(null)

  const load = async () => {
    const res = await listProviders()
    setItems(res.data ?? [])
  }

  useEffect(() => {
    let cancelled = false
    const run = async () => {
      try {
        const res = await listProviders()
        if (!cancelled) setItems(res.data ?? [])
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
        item.base_url.toLowerCase().includes(q),
    )
    next.sort((a, b) =>
      sort === "name-asc" ? a.name.localeCompare(b.name) : b.name.localeCompare(a.name),
    )
    return next
  }, [items, search, sort])

  const openCreate = () => {
    setEditing(null)
    setName("")
    setBaseUrl("")
    setApiKey("")
    setEnabled(true)
    setAuthScheme("bearer")
    setAuthHeader("")
    setHeadersText("")
    setChatPath("")
    setOpen(true)
  }

  const openEdit = (item: Provider) => {
    setEditing(item)
    setName(item.name)
    setBaseUrl(item.base_url)
    setApiKey("")
    setEnabled(item.enabled)
    setAuthScheme(item.auth_scheme || "bearer")
    setAuthHeader(item.auth_header ?? "")
    setHeadersText(formatHeaders(item.extra_headers))
    setChatPath(item.chat_path ?? "")
    setOpen(true)
  }

  const handleSave = async () => {
    try {
      const payload = {
        name,
        base_url: baseUrl,
        api_key: apiKey,
        enabled,
        auth_scheme: authScheme,
        auth_header: authHeader,
        extra_headers: parseHeaders(headersText),
        chat_path: chatPath,
      }
      if (editing) await updateProvider(editing.id, payload)
      else await createProvider(payload)
      setOpen(false)
      await load()
    } catch (err) {
      toast.error((err as Error).message)
    }
  }

  return (
    <PageShell>
      <PageContainer>
        <PageHeader
          title={t("providers.title")}
          action={<Button onClick={openCreate}>{t("providers.create")}</Button>}
        />
        <EntityListToolbar
          searchValue={search}
          onSearchChange={setSearch}
          searchPlaceholder={t("providers.searchPlaceholder")}
          sort={
            <Select
              value={sort}
              onChange={(value) => setSort(value as SortKey)}
              options={[
                { value: "name-asc", label: t("providers.sortNameAsc") },
                { value: "name-desc", label: t("providers.sortNameDesc") },
              ]}
            />
          }
          resultCount={t("common.total").replace("{n}", String(filtered.length))}
        />
        <Card className="overflow-hidden py-0">
          {filtered.length === 0 ? (
            <p className="px-6 py-12 text-center text-sm text-muted-foreground">{t("providers.empty")}</p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t("providers.name")}</TableHead>
                  <TableHead>{t("providers.baseUrl")}</TableHead>
                  <TableHead>{t("common.status")}</TableHead>
                  <TableHead className="text-right">{t("common.actions")}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {filtered.map((item) => (
                  <TableRow key={item.id}>
                    <TableCell className="font-medium">{item.name}</TableCell>
                    <TableCell className="font-mono text-xs">{item.base_url}</TableCell>
                    <TableCell>
                      <Badge variant={item.enabled ? "success" : "inactive"}>
                        {item.enabled ? t("common.enabled") : t("common.disabled")}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-right">
                      <Button variant="ghost" size="sm" onClick={() => openEdit(item)}>
                        {t("common.edit")}
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
          <DialogContent className="flex max-h-[85vh] w-full max-w-lg flex-col overflow-hidden">
            <DialogHeader>
              <DialogTitle>{editing ? t("providers.edit") : t("providers.create")}</DialogTitle>
            </DialogHeader>
            <div className="min-h-0 flex-1 space-y-4 overflow-y-auto">
              <div className="space-y-2">
                <Label htmlFor="provider-name">{t("providers.name")}</Label>
                <Input id="provider-name" value={name} onChange={(e) => setName(e.target.value)} />
              </div>
              <div className="space-y-2">
                <Label htmlFor="provider-url">{t("providers.baseUrl")}</Label>
                <Input id="provider-url" value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} />
              </div>
              <div className="space-y-2">
                <Label htmlFor="provider-key">{t("providers.apiKey")}</Label>
                <Input
                  id="provider-key"
                  type="password"
                  value={apiKey}
                  onChange={(e) => setApiKey(e.target.value)}
                />
                {editing ? <p className="text-meta-sm text-muted-foreground">{t("providers.apiKeyHint")}</p> : null}
              </div>
              <div className="space-y-3 rounded-lg bg-muted/30 p-3">
                <div className="space-y-2">
                  <Label>{t("providers.authScheme")}</Label>
                  <Select
                    value={authScheme}
                    onChange={setAuthScheme}
                    options={[
                      { value: "bearer", label: t("providers.authSchemeBearer") },
                      { value: "raw", label: t("providers.authSchemeRaw") },
                      { value: "none", label: t("providers.authSchemeNone") },
                    ]}
                  />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="provider-auth-header">{t("providers.authHeader")}</Label>
                  <Input
                    id="provider-auth-header"
                    value={authHeader}
                    placeholder="authorization"
                    onChange={(e) => setAuthHeader(e.target.value)}
                  />
                  <p className="text-meta-sm text-muted-foreground">{t("providers.authHeaderHint")}</p>
                </div>
                <div className="space-y-2">
                  <Label htmlFor="provider-headers">{t("providers.extraHeaders")}</Label>
                  <textarea
                    id="provider-headers"
                    value={headersText}
                    onChange={(e) => setHeadersText(e.target.value)}
                    className="min-h-20 w-full rounded-lg bg-muted/30 px-3 py-2 font-mono text-xs text-foreground"
                  />
                  <p className="text-meta-sm text-muted-foreground">{t("providers.extraHeadersHint")}</p>
                </div>
                <div className="space-y-2">
                  <Label htmlFor="provider-chat-path">{t("providers.chatPath")}</Label>
                  <Input
                    id="provider-chat-path"
                    value={chatPath}
                    onChange={(e) => setChatPath(e.target.value)}
                  />
                  <p className="text-meta-sm text-muted-foreground">{t("providers.chatPathHint")}</p>
                </div>
              </div>
              <div className="flex items-center justify-between rounded-lg bg-muted/30 px-3 py-2">
                <Label htmlFor="provider-enabled">{t("common.enabled")}</Label>
                <Switch id="provider-enabled" checked={enabled} onCheckedChange={setEnabled} />
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

        <ConfirmAlertDialog
          open={Boolean(pendingDelete)}
          onOpenChange={(next) => {
            if (!next) setPendingDelete(null)
          }}
          title={t("providers.deleteTitle")}
          description={t("providers.deleteDescription")}
          cancelText={t("common.cancel")}
          confirmText={t("common.delete")}
          confirmClassName="bg-destructive text-destructive-foreground"
          onConfirm={() => {
            if (!pendingDelete) return
            void deleteProvider(pendingDelete.id)
              .then(() => load())
              .catch((err: Error) => toast.error(err.message))
            setPendingDelete(null)
          }}
        />
      </PageContainer>
    </PageShell>
  )
}

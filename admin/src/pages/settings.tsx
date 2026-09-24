import { useEffect, useRef, useState } from "react"
import { toast } from "sonner"
import { PageContainer } from "@/components/layout/page-container"
import { PageHeader } from "@/components/layout/page-header"
import { PageShell } from "@/components/layout/page-shell"
import { ConfirmAlertDialog } from "@/components/ui/confirm-alert-dialog"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Label } from "@/components/ui/label"
import { t } from "@/lib/i18n"
import { copyTextToClipboard } from "@/lib/utils"
import { getOverview } from "@/lib/resources/overview"
import { exportCatalog, importCatalog } from "@/lib/resources/catalog"
import type { CreatedSecret } from "@/lib/api-types/catalog"
import type { Overview } from "@/lib/api-types/overview"

export function SettingsPage() {
  const [data, setData] = useState<Overview | null>(null)
  const [importOpen, setImportOpen] = useState(false)
  const [yamlText, setYamlText] = useState("")
  const [replace, setReplace] = useState(false)
  const [confirmReplace, setConfirmReplace] = useState(false)
  const [secrets, setSecrets] = useState<CreatedSecret[]>([])
  const fileRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    let cancelled = false
    const run = async () => {
      try {
        const res = await getOverview()
        if (!cancelled && res.success && res.data) setData(res.data)
      } catch {
        if (!cancelled) setData(null)
      }
    }
    void run()
    return () => {
      cancelled = true
    }
  }, [])

  const storageLabel = (kind: Overview["storage"] | undefined) => {
    if (kind === "redis") return t("settings.storageRedis")
    if (kind === "memory") return t("settings.storageMemory")
    if (kind === "sqlite") return t("settings.storageSqlite")
    return "—"
  }

  const rows = [
    { label: t("settings.version"), value: data?.version ?? "—" },
    { label: t("settings.listen"), value: data?.listen ?? "—" },
    { label: t("settings.storage"), value: storageLabel(data?.storage) },
    { label: t("settings.h2c"), value: data?.h2c ? t("settings.h2cOn") : t("settings.h2cOff") },
    { label: t("settings.routing"), value: t("settings.routingValue") },
  ]

  const handleExport = async () => {
    try {
      const res = await exportCatalog()
      const text = res.data?.yaml ?? ""
      const blob = new Blob([text], { type: "text/yaml" })
      const url = URL.createObjectURL(blob)
      const link = document.createElement("a")
      link.href = url
      link.download = "xlite.yaml"
      link.click()
      URL.revokeObjectURL(url)
    } catch (err) {
      toast.error((err as Error).message)
    }
  }

  const runImport = async () => {
    try {
      const res = await importCatalog(yamlText, replace)
      setImportOpen(false)
      setConfirmReplace(false)
      if (res.data?.secrets?.length) setSecrets(res.data.secrets)
      toast.success(t("settings.catalogImported"))
      const overview = await getOverview()
      if (overview.success && overview.data) setData(overview.data)
    } catch (err) {
      toast.error((err as Error).message)
    }
  }

  const submitImport = () => {
    if (replace) {
      setConfirmReplace(true)
      return
    }
    void runImport()
  }

  return (
    <PageShell>
      <PageContainer>
        <PageHeader title={t("settings.title")} />
        <Card>
          <CardContent className="space-y-4 px-6 py-5">
            {rows.map((row) => (
              <div key={row.label} className="rounded-lg bg-muted/30 px-4 py-3">
                <div className="text-label-sm text-muted-foreground">{row.label}</div>
                <div className="mt-1 text-sm text-foreground">{row.value}</div>
              </div>
            ))}
            <div className="rounded-lg bg-muted/30 px-4 py-3">
              <div className="text-label-sm text-muted-foreground">{t("settings.password")}</div>
              <div className="mt-1 text-sm text-foreground">{t("settings.passwordHint")}</div>
            </div>
          </CardContent>
        </Card>

        <Card className="mt-6">
          <CardContent className="space-y-4 px-6 py-5">
            <div>
              <div className="text-sm font-medium text-foreground">{t("settings.catalog")}</div>
              <p className="mt-1 text-sm text-muted-foreground">{t("settings.catalogHint")}</p>
            </div>
            <div className="flex flex-wrap gap-2">
              <Button onClick={() => setImportOpen(true)}>{t("settings.catalogImport")}</Button>
              <Button variant="outline" onClick={() => void handleExport()}>
                {t("settings.catalogExport")}
              </Button>
            </div>
          </CardContent>
        </Card>

        <Dialog open={importOpen} onOpenChange={setImportOpen}>
          <DialogContent className="flex max-h-[85vh] w-full max-w-2xl flex-col overflow-hidden">
            <DialogHeader>
              <DialogTitle>{t("settings.catalogImport")}</DialogTitle>
            </DialogHeader>
            <div className="min-h-0 flex-1 space-y-4 overflow-y-auto">
              <div className="flex flex-wrap gap-2">
                <input
                  ref={fileRef}
                  type="file"
                  accept=".yaml,.yml,text/yaml"
                  className="hidden"
                  onChange={(event) => {
                    const file = event.target.files?.[0]
                    if (!file) return
                    void file.text().then(setYamlText)
                  }}
                />
                <Button variant="outline" onClick={() => fileRef.current?.click()}>
                  {t("settings.catalogFile")}
                </Button>
              </div>
              <div className="space-y-2">
                <Label htmlFor="catalog-yaml">{t("settings.catalogPaste")}</Label>
                <textarea
                  id="catalog-yaml"
                  value={yamlText}
                  onChange={(event) => setYamlText(event.target.value)}
                  className="min-h-40 w-full rounded-lg bg-muted/30 px-3 py-2 font-mono text-xs text-foreground"
                />
              </div>
              <div className="flex flex-col gap-2">
                <label className="flex items-center gap-2 text-sm">
                  <input
                    type="radio"
                    name="import-mode"
                    checked={!replace}
                    onChange={() => setReplace(false)}
                  />
                  {t("settings.catalogMerge")}
                </label>
                <label className="flex items-center gap-2 text-sm">
                  <input
                    type="radio"
                    name="import-mode"
                    checked={replace}
                    onChange={() => setReplace(true)}
                  />
                  {t("settings.catalogReplace")}
                </label>
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setImportOpen(false)}>
                {t("common.cancel")}
              </Button>
              <Button onClick={submitImport} disabled={!yamlText.trim()}>
                {t("settings.catalogImport")}
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        <ConfirmAlertDialog
          open={confirmReplace}
          onOpenChange={setConfirmReplace}
          title={t("settings.catalogReplaceTitle")}
          description={t("settings.catalogReplaceDescription")}
          cancelText={t("common.cancel")}
          confirmText={t("settings.catalogImport")}
          confirmClassName="bg-destructive text-destructive-foreground"
          onConfirm={() => {
            void runImport()
          }}
        />

        <Dialog open={secrets.length > 0} onOpenChange={(open) => !open && setSecrets([])}>
          <DialogContent className="w-full max-w-2xl">
            <DialogHeader>
              <DialogTitle>{t("settings.catalogSecretTitle")}</DialogTitle>
            </DialogHeader>
            <div className="space-y-3">
              <p className="text-sm text-muted-foreground">{t("keys.onceHint")}</p>
              {secrets.map((item) => (
                <div key={item.name} className="rounded-lg bg-muted/30 p-3">
                  <div className="text-sm font-medium">{item.name}</div>
                  <div className="mt-1 break-all font-mono text-xs">{item.key}</div>
                  <Button
                    className="mt-2"
                    variant="outline"
                    size="sm"
                    onClick={() => {
                      void copyTextToClipboard(item.key).then(() => toast.success(t("common.copied")))
                    }}
                  >
                    {t("common.copy")}
                  </Button>
                </div>
              ))}
            </div>
            <DialogFooter>
              <Button onClick={() => setSecrets([])}>{t("common.done")}</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      </PageContainer>
    </PageShell>
  )
}

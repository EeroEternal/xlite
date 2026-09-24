import { useEffect, useState } from "react"
import { PageContainer } from "@/components/layout/page-container"
import { PageHeader } from "@/components/layout/page-header"
import { PageShell } from "@/components/layout/page-shell"
import { Card, CardContent } from "@/components/ui/card"
import { t } from "@/lib/i18n"
import { getOverview } from "@/lib/resources/overview"
import type { Overview } from "@/lib/api-types/overview"

export function SettingsPage() {
  const [data, setData] = useState<Overview | null>(null)

  useEffect(() => {
    getOverview()
      .then((res) => {
        if (res.success && res.data) setData(res.data)
      })
      .catch(() => setData(null))
  }, [])

  const rows = [
    { label: t("settings.listen"), value: data?.listen ?? "—" },
    { label: t("settings.h2c"), value: data?.h2c ? t("settings.h2cOn") : t("settings.h2cOff") },
    { label: t("settings.routing"), value: t("settings.routingValue") },
  ]

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
      </PageContainer>
    </PageShell>
  )
}

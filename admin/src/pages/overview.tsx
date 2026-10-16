import { useEffect, useState } from "react"
import { KeyRound, Server, Waypoints, Workflow } from "lucide-react"
import { PageContainer } from "@/components/layout/page-container"
import { PageHeader } from "@/components/layout/page-header"
import { PageShell } from "@/components/layout/page-shell"
import { Card, CardContent } from "@/components/ui/card"
import { t } from "@/lib/i18n"
import { getOverview } from "@/lib/resources/overview"
import type { Overview } from "@/lib/api-types/overview"

export function OverviewPage() {
  const [data, setData] = useState<Overview | null>(null)

  useEffect(() => {
    getOverview()
      .then((res) => {
        if (res.success && res.data) setData(res.data)
      })
      .catch(() => setData(null))
  }, [])

  const cards = [
    { key: "overview.providers", value: data?.providers ?? "—", icon: Server },
    { key: "overview.models", value: data?.models ?? "—", icon: Waypoints },
    { key: "overview.channels", value: data?.channels ?? "—", icon: Workflow },
    { key: "overview.apiKeys", value: data?.api_keys ?? "—", icon: KeyRound },
  ]

  return (
    <PageShell>
      <PageContainer>
        <PageHeader title={t("overview.title")} />
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
          {cards.map((card) => (
            <Card key={card.key}>
              <CardContent className="flex items-start justify-between px-5 py-5">
                <div>
                  <div className="text-label-sm text-muted-foreground">{t(card.key)}</div>
                  <div className="mt-2 text-metric text-foreground">{card.value}</div>
                </div>
                <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-muted text-primary">
                  <card.icon className="h-4 w-4" aria-hidden="true" />
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      </PageContainer>
    </PageShell>
  )
}

import { TrendingDown, TrendingUp } from "lucide-react"
import type { FC, SVGProps } from "react"
import { Card, CardContent } from "@/components/ui/card"
import { cn } from "@/lib/utils"

export interface StatCardProps {
  title: string
  value: string | number
  subtitle?: string
  icon?: FC<SVGProps<SVGSVGElement>>
  trend?: {
    value: string
    isPositive: boolean
    label?: string
  }
  valueClassName?: string
  iconClassName?: string
  iconContainerClassName?: string
  className?: string
}

export function StatCard({
  title,
  value,
  subtitle,
  icon: Icon,
  trend,
  valueClassName,
  iconClassName,
  iconContainerClassName,
  className,
}: StatCardProps) {
  return (
    <Card className={cn("h-full overflow-hidden border-border py-0 shadow-none", className)}>
      <CardContent className="flex h-full flex-col px-5 py-5">
        <div className="flex min-w-0 items-start justify-between gap-3">
          <div className="min-w-0 space-y-2">
            <span className="text-label-sm line-clamp-2 block min-h-[2.5rem] break-words text-muted-foreground">
              {title}
            </span>
            <div
              className={cn(
                "text-metric break-words tracking-tight text-foreground",
                valueClassName,
              )}
            >
              {value}
            </div>
          </div>
          {Icon ? (
            <div
              className={cn(
                "flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-muted text-primary",
                iconContainerClassName,
              )}
            >
              <Icon className={cn("h-4 w-4", iconClassName)} aria-hidden="true" />
            </div>
          ) : null}
        </div>
        <div className="flex-grow" />
        <div className="mt-4 min-h-[2.5rem] border-t border-border pt-3">
          {trend ? (
            <span
              className={cn(
                "flex min-w-0 flex-wrap items-center gap-1 text-label-sm",
                trend.isPositive ? "text-success" : "text-destructive",
              )}
            >
              {trend.isPositive ? (
                <TrendingUp className="h-3 w-3 shrink-0" />
              ) : (
                <TrendingDown className="h-3 w-3 shrink-0" />
              )}
              <span className="shrink-0">{trend.value}</span>
              <span className="text-meta-sm break-words font-normal text-muted-foreground">
                {trend.label}
              </span>
            </span>
          ) : subtitle ? (
            <span className="text-meta-sm line-clamp-2 block break-words text-muted-foreground">
              {subtitle}
            </span>
          ) : null}
        </div>
      </CardContent>
    </Card>
  )
}

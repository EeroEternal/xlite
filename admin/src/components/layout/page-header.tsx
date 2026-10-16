import type { ReactNode } from "react"
import { cn } from "@/lib/utils"

interface PageHeaderProps {
    title?: string
    description?: string
    action?: ReactNode
    className?: string
}

export function PageHeader({ title, description, action, className }: PageHeaderProps) {
    return (
        <div className={cn("mb-6 flex w-full items-center justify-between gap-3", description && "items-start", className)}>
            <div className="min-w-0">
                {title ? (
                    <h1 className="min-w-0 break-words text-page-title tracking-tight text-foreground">
                        {title}
                    </h1>
                ) : null}
                {description ? (
                    <p className="mt-1 text-sm text-muted-foreground">{description}</p>
                ) : null}
            </div>
            {action ? (
                <div className="flex shrink-0 flex-nowrap items-center justify-end gap-2">
                    {action}
                </div>
            ) : null}
        </div>
    )
}

import { Check, Monitor, Moon, Sun } from "lucide-react"

import { Button } from "@/components/ui/button"
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { t } from "@/lib/i18n"
import { type ThemePreference, useTheme } from "@/lib/theme-context"

const options: { value: ThemePreference; labelKey: string; icon: typeof Sun }[] = [
  { value: "light", labelKey: "theme.light", icon: Sun },
  { value: "dark", labelKey: "theme.dark", icon: Moon },
  { value: "system", labelKey: "theme.system", icon: Monitor },
]

export function ThemeSwitcher() {
  const { preference, resolvedTheme, setPreference } = useTheme()
  const TriggerIcon = resolvedTheme === "dark" ? Moon : Sun
  const label = t("theme.toggle")

  return (
    <DropdownMenu>
      <Tooltip>
        <TooltipTrigger asChild>
          <DropdownMenuTrigger asChild>
            <Button variant="ghost" size="icon-sm" aria-label={label}>
              <TriggerIcon aria-hidden="true" />
            </Button>
          </DropdownMenuTrigger>
        </TooltipTrigger>
        <TooltipContent>{label}</TooltipContent>
      </Tooltip>
      <DropdownMenuContent align="end" className="z-50 min-w-36">
        {options.map((option) => {
          const Icon = option.icon
          return (
            <DropdownMenuItem
              key={option.value}
              onClick={() => setPreference(option.value)}
              className="flex items-center gap-2"
            >
              <Icon className="h-4 w-4" aria-hidden="true" />
              <span className="flex-1">{t(option.labelKey)}</span>
              {preference === option.value && <Check className="h-4 w-4" aria-hidden="true" />}
            </DropdownMenuItem>
          )
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

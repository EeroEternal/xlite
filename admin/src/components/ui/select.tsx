import * as React from "react"
import * as SelectPrimitive from "@radix-ui/react-select"
import { cn } from "@/lib/utils"
import { ChevronDown, Check } from "lucide-react"
import { t } from "@/lib/i18n"

interface SelectOption {
  value: string
  label: string
  /** Optional section header. Option labels stay identity-only. */
  group?: string
}

function groupedSelectOptions(options: SelectOption[]): Array<{
  group?: string
  options: SelectOption[]
}> {
  const showGroups = options.some((option) => option.group)
  if (!showGroups) {
    return [{ options }]
  }
  const sections: Array<{ group?: string; options: SelectOption[] }> = []
  const indexByGroup = new Map<string, number>()
  for (const option of options) {
    const group = option.group ?? ""
    const existing = indexByGroup.get(group)
    if (existing == null) {
      indexByGroup.set(group, sections.length)
      sections.push({ group: group || undefined, options: [option] })
    } else {
      sections[existing].options.push(option)
    }
  }
  return sections
}

interface SelectProps {
  value?: string
  onChange?: (value: string) => void
  options: SelectOption[]
  placeholder?: string
  className?: string
  triggerClassName?: string
  icon?: React.ReactNode
  id?: string
  menuSide?: "top" | "bottom"
  emptyText?: string
  disabled?: boolean
}

const Select = ({
  value,
  onChange,
  options,
  placeholder = t('common.selectPlaceholder'),
  className,
  triggerClassName,
  icon,
  id,
  menuSide = "bottom",
  emptyText,
  disabled,
}: SelectProps) => {
  return (
    <SelectPrimitive.Root value={value} onValueChange={onChange} disabled={disabled}>
      <div className={cn("relative", className)}>
        <SelectPrimitive.Trigger
          id={id}
          className={cn(
            "flex h-8 w-full items-center justify-between rounded-lg border border-input bg-background px-3 text-sm ring-offset-background hover:bg-muted focus:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50",
            triggerClassName
          )}
        >
          <div className="flex min-w-0 flex-1 items-center gap-1 overflow-hidden">
            {icon}
            <SelectPrimitive.Value
              className="block truncate"
              placeholder={placeholder}
            />
          </div>
          <SelectPrimitive.Icon asChild>
            <ChevronDown className="ml-1 h-3.5 w-3.5 shrink-0 opacity-50" />
          </SelectPrimitive.Icon>
        </SelectPrimitive.Trigger>
      </div>
      <SelectPrimitive.Portal>
        <SelectPrimitive.Content
          side={menuSide}
          position="popper"
          className="z-[150] max-h-[240px] min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded-lg border bg-popover shadow-md"
        >
          <SelectPrimitive.Viewport className="max-h-[240px] p-2">
            {options.length === 0 ? (
              <div className="px-2 py-2 text-sm text-muted-foreground">
                {emptyText || t("common.empty")}
              </div>
            ) : (
              groupedSelectOptions(options).map((section) => (
                <SelectPrimitive.Group key={section.group ?? "__ungrouped"}>
                  {section.group ? (
                    <SelectPrimitive.Label className="px-2 py-1.5 text-xs font-medium text-muted-foreground">
                      {section.group}
                    </SelectPrimitive.Label>
                  ) : null}
                  {section.options.map((option) => (
                    <SelectPrimitive.Item
                      key={option.value}
                      value={option.value}
                      className="relative flex cursor-default select-none items-center rounded-sm py-1.5 pl-2 pr-8 text-sm outline-none hover:bg-muted focus:bg-muted focus:text-foreground"
                    >
                      <SelectPrimitive.ItemText>
                        <span className="block whitespace-nowrap">{option.label}</span>
                      </SelectPrimitive.ItemText>
                      <SelectPrimitive.ItemIndicator className="absolute right-2 inline-flex items-center">
                        <Check className="h-4 w-4" />
                      </SelectPrimitive.ItemIndicator>
                    </SelectPrimitive.Item>
                  ))}
                </SelectPrimitive.Group>
              ))
            )}
          </SelectPrimitive.Viewport>
        </SelectPrimitive.Content>
      </SelectPrimitive.Portal>
    </SelectPrimitive.Root>
  )
}

export { Select }
export type { SelectOption }

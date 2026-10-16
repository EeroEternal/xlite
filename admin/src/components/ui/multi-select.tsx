import * as React from "react"
import { Check, ChevronDown, X } from "lucide-react"
import * as PopoverPrimitive from "@radix-ui/react-popover"
import { useDialogPortalContainer } from "@/components/ui/dialog-portal-context"
import { cn } from "@/lib/utils"

interface MultiSelectOption {
  value: number
  label: string
  disabled?: boolean
}

interface MultiSelectProps {
  options: MultiSelectOption[]
  value: number[]
  onChange: (value: number[]) => void
  placeholder?: string
  className?: string
  /**
   * Fixed height of the closed trigger chip area (scroll inside).
   * Keep this fixed to avoid layout jitter when selection count changes.
   */
  triggerHeightClassName?: string
  /** Prefer opening above the trigger so the menu does not cover help/meta below. */
  side?: "top" | "bottom"
}

export function MultiSelect({
  options,
  value,
  onChange,
  placeholder = "Select options...",
  className,
  triggerHeightClassName = "h-10",
  side = "top",
}: MultiSelectProps) {
  const [open, setOpen] = React.useState(false)
  const dialogPortal = useDialogPortalContainer()
  const triggerRef = React.useRef<HTMLDivElement>(null)
  const contentRef = React.useRef<HTMLDivElement>(null)

  React.useEffect(() => {
    if (!open) return

    const handlePointerDown = (event: PointerEvent) => {
      const target = event.target as Node
      if (contentRef.current?.contains(target)) return
      setOpen(false)
    }

    // Defer so the pointerdown that opened the menu does not immediately close it.
    const timeoutId = window.setTimeout(() => {
      document.addEventListener("pointerdown", handlePointerDown, true)
    }, 0)

    return () => {
      window.clearTimeout(timeoutId)
      document.removeEventListener("pointerdown", handlePointerDown, true)
    }
  }, [open])

  const handleUnselect = (optionValue: number) => {
    onChange(value.filter((v) => v !== optionValue))
  }

  const toggleOption = (optionValue: number) => {
    const option = options.find((item) => item.value === optionValue)
    if (option?.disabled && !value.includes(optionValue)) {
      return
    }
    if (value.includes(optionValue)) {
      handleUnselect(optionValue)
    } else {
      onChange([...value, optionValue])
    }
  }

  const selectedOptions = options.filter((option) => value.includes(option.value))

  const popoverContent = (
    <PopoverPrimitive.Content
      ref={contentRef}
      className={cn(
        "w-[min(var(--radix-popover-trigger-width),calc(100vw-2rem))] min-w-[200px] max-w-[calc(100vw-2rem)] overflow-hidden rounded-md border bg-popover p-1 text-popover-foreground shadow-md outline-none",
        dialogPortal ? "z-20" : "z-[200]"
      )}
      align="start"
      side={side}
      sideOffset={6}
      collisionPadding={
        dialogPortal ? { top: 72, bottom: 8, left: 8, right: 8 } : 8
      }
      avoidCollisions
      onOpenAutoFocus={(event) => event.preventDefault()}
      onCloseAutoFocus={(event) => event.preventDefault()}
      onPointerDownOutside={() => setOpen(false)}
      onFocusOutside={() => setOpen(false)}
    >
      <div className="max-h-60 overflow-y-auto">
        {options.map((option) => {
          const selected = value.includes(option.value)
          const disabled = Boolean(option.disabled) && !selected
          return (
            <div
              key={option.value}
              role="option"
              aria-selected={selected}
              aria-disabled={disabled}
              className={cn(
                "relative flex cursor-pointer select-none items-start rounded-sm px-2 py-1.5 text-sm outline-none",
                disabled
                  ? "cursor-not-allowed opacity-40"
                  : "hover:bg-accent hover:text-accent-foreground",
                selected && !disabled && "bg-accent/50"
              )}
              onPointerDown={(event) => {
                event.preventDefault()
                event.stopPropagation()
              }}
              onClick={(event) => {
                event.preventDefault()
                event.stopPropagation()
                if (!disabled) toggleOption(option.value)
              }}
            >
              <div
                className={cn(
                  "mr-2 flex h-4 w-4 items-center justify-center rounded-sm border border-primary",
                  selected
                    ? "bg-primary text-primary-foreground"
                    : "opacity-50 [&_svg]:invisible"
                )}
              >
                <Check className={cn("h-4 w-4")} />
              </div>
              <span className="break-words leading-5">{option.label}</span>
            </div>
          )
        })}
      </div>
    </PopoverPrimitive.Content>
  )

  return (
    <PopoverPrimitive.Root open={open} onOpenChange={setOpen} modal={false}>
      <PopoverPrimitive.Trigger asChild>
        <div
          ref={triggerRef}
          role="combobox"
          aria-expanded={open}
          onPointerDown={(event) => {
            if (open) {
              event.preventDefault()
              setOpen(false)
            }
          }}
          className={cn(
            "flex w-full min-w-0 cursor-pointer items-center justify-between rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50",
            triggerHeightClassName,
            className
          )}
        >
          <div className="flex min-h-0 min-w-0 flex-1 items-center gap-1 overflow-x-auto overflow-y-hidden">
            {selectedOptions.length > 0 ? (
              selectedOptions.map((option) => (
                <span
                  key={option.value}
                  className="inline-flex shrink-0 items-center gap-1 overflow-hidden rounded-md border border-transparent bg-secondary px-2 py-0.5 text-xs font-medium text-secondary-foreground"
                  title={option.label}
                >
                  <span className="max-w-[10rem] truncate">{option.label}</span>
                  <X
                    className="h-3 w-3 shrink-0 cursor-pointer hover:text-destructive"
                    onClick={(e) => {
                      e.preventDefault()
                      e.stopPropagation()
                      handleUnselect(option.value)
                    }}
                    onPointerDown={(e) => {
                      e.preventDefault()
                      e.stopPropagation()
                    }}
                  />
                </span>
              ))
            ) : (
              <span className="truncate text-muted-foreground">{placeholder}</span>
            )}
          </div>
          <ChevronDown className="ml-2 h-3.5 w-3.5 shrink-0 opacity-50" />
        </div>
      </PopoverPrimitive.Trigger>
      {dialogPortal ? (
        <PopoverPrimitive.Portal container={dialogPortal}>
          {popoverContent}
        </PopoverPrimitive.Portal>
      ) : (
        <PopoverPrimitive.Portal>{popoverContent}</PopoverPrimitive.Portal>
      )}
    </PopoverPrimitive.Root>
  )
}

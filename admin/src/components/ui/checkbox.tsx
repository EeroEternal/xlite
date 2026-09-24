import * as React from "react"
import { Check } from "lucide-react"
import { cn } from "@/lib/utils"

interface CheckboxProps {
  checked?: boolean
  onCheckedChange?: (checked: boolean) => void
  disabled?: boolean
  className?: string
  id?: string
  "aria-label"?: string
}

const Checkbox = React.forwardRef<HTMLButtonElement, CheckboxProps>(
  (
    {
      checked = false,
      onCheckedChange,
      disabled = false,
      className,
      id,
      "aria-label": ariaLabel,
    },
    ref
  ) => {
    return (
      <button
        ref={ref}
        id={id}
        type="button"
        role="checkbox"
        aria-checked={checked}
        aria-label={ariaLabel}
        disabled={disabled}
        onClick={(e) => {
          e.stopPropagation()
          onCheckedChange?.(!checked)
        }}
        className={cn(
          "flex h-4 w-4 shrink-0 items-center justify-center rounded-sm border border-primary shadow-sm transition-colors",
          "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2",
          "disabled:cursor-not-allowed disabled:opacity-50",
          checked ? "bg-primary text-primary-foreground" : "bg-background",
          className
        )}
      >
        {checked ? <Check className="h-3 w-3 stroke-[3]" /> : null}
      </button>
    )
  }
)
Checkbox.displayName = "Checkbox"

export { Checkbox }

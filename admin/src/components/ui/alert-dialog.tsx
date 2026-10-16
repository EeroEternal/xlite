import * as React from "react"
import { createPortal } from "react-dom"
import { cn } from "@/lib/utils"
import { Button } from "@/components/ui/button"

interface AlertDialogProps {
  open?: boolean
  onOpenChange?: (open: boolean) => void
  children: React.ReactNode
}

const FOCUSABLE_SELECTOR = [
  'a[href]',
  'button:not([disabled])',
  'textarea:not([disabled])',
  'input:not([disabled])',
  'select:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(",")

function getFocusable(container: HTMLElement) {
  return Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)).filter(
    (el) => !el.hasAttribute("disabled") && el.getAttribute("aria-hidden") !== "true"
  )
}

type AlertDialogContentContextValue = {
  setContentNode: (node: HTMLDivElement | null) => void
}

const AlertDialogContentContext = React.createContext<AlertDialogContentContextValue | null>(null)

const AlertDialog = ({ open, onOpenChange, children }: AlertDialogProps) => {
  const contentRef = React.useRef<HTMLDivElement | null>(null)
  const previouslyFocusedRef = React.useRef<HTMLElement | null>(null)
  const setContentNode = React.useCallback((node: HTMLDivElement | null) => {
    contentRef.current = node
  }, [])

  React.useEffect(() => {
    if (open) {
      document.body.style.overflow = "hidden"
    } else {
      document.body.style.overflow = ""
    }
    return () => {
      document.body.style.overflow = ""
    }
  }, [open])

  React.useEffect(() => {
    if (!open) return

    previouslyFocusedRef.current = document.activeElement as HTMLElement | null
    const focusAction = () => {
      const root = contentRef.current
      if (!root) return
      const focusable = getFocusable(root)
      ;(focusable[focusable.length - 1] ?? root).focus()
    }
    const rafId = window.requestAnimationFrame(focusAction)

    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault()
        e.stopPropagation()
        onOpenChange?.(false)
        return
      }

      if (e.key !== "Tab" || !contentRef.current) return
      const focusable = getFocusable(contentRef.current)
      if (focusable.length === 0) {
        e.preventDefault()
        contentRef.current.focus()
        return
      }
      const first = focusable[0]
      const last = focusable[focusable.length - 1]
      const active = document.activeElement as HTMLElement | null
      if (e.shiftKey && active === first) {
        e.preventDefault()
        last.focus()
      } else if (!e.shiftKey && active === last) {
        e.preventDefault()
        first.focus()
      }
    }

    document.addEventListener("keydown", handleKeyDown, true)
    return () => {
      window.cancelAnimationFrame(rafId)
      document.removeEventListener("keydown", handleKeyDown, true)
      previouslyFocusedRef.current?.focus?.()
    }
  }, [open, onOpenChange])

  if (!open) return null

  return createPortal(
    <AlertDialogContentContext.Provider value={{ setContentNode }}>
      <div className="fixed inset-0 z-[100] flex items-center justify-center p-4">
        <div
          className="fixed inset-0 bg-black/50 backdrop-blur-sm transition-all"
          onClick={() => onOpenChange?.(false)}
        />
        <div className="relative z-[101] w-full max-w-lg pointer-events-none">
          <div className="pointer-events-auto">{children}</div>
        </div>
      </div>
    </AlertDialogContentContext.Provider>,
    document.body
  )
}

const AlertDialogContent = React.forwardRef<HTMLDivElement, React.HTMLAttributes<HTMLDivElement>>(
  ({ className, children, ...props }, ref) => {
    const ctx = React.useContext(AlertDialogContentContext)
    const setRefs = React.useCallback(
      (node: HTMLDivElement | null) => {
        ctx?.setContentNode(node)
        if (typeof ref === "function") ref(node)
        else if (ref) ref.current = node
      },
      [ctx, ref]
    )

    return (
      <div
        ref={setRefs}
        role="alertdialog"
        aria-modal="true"
        tabIndex={-1}
        className={cn(
          // Viewport bounds (design.md hard rule 5): capped at 85vh with an
          // internal vertical scrollbar by default.
          "max-h-[85vh] w-full overflow-y-auto rounded-lg border bg-background p-6 shadow-lg outline-none",
          className
        )}
        {...props}
      >
        {children}
      </div>
    )
  }
)
AlertDialogContent.displayName = "AlertDialogContent"

const AlertDialogHeader = ({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) => (
  <div className={cn("mb-4 flex flex-col space-y-2 text-center sm:text-left", className)} {...props} />
)
AlertDialogHeader.displayName = "AlertDialogHeader"

const AlertDialogFooter = ({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) => (
  <div
    className={cn("mt-6 flex flex-col-reverse sm:flex-row sm:justify-end sm:space-x-2", className)}
    {...props}
  />
)
AlertDialogFooter.displayName = "AlertDialogFooter"

const AlertDialogTitle = React.forwardRef<HTMLHeadingElement, React.HTMLAttributes<HTMLHeadingElement>>(
  ({ className, ...props }, ref) => (
    <h2 ref={ref} className={cn("text-lg font-semibold", className)} {...props} />
  )
)
AlertDialogTitle.displayName = "AlertDialogTitle"

const AlertDialogDescription = React.forwardRef<
  HTMLParagraphElement,
  React.HTMLAttributes<HTMLParagraphElement>
>(({ className, ...props }, ref) => (
  <p ref={ref} className={cn("text-sm text-muted-foreground", className)} {...props} />
))
AlertDialogDescription.displayName = "AlertDialogDescription"

const AlertDialogAction = React.forwardRef<
  HTMLButtonElement,
  React.ComponentPropsWithoutRef<typeof Button>
>(({ className, ...props }, ref) => <Button ref={ref} className={className} {...props} />)
AlertDialogAction.displayName = "AlertDialogAction"

const AlertDialogCancel = React.forwardRef<
  HTMLButtonElement,
  React.ComponentPropsWithoutRef<typeof Button>
>(({ className, ...props }, ref) => (
  <Button ref={ref} variant="outline" className={className} {...props} />
))
AlertDialogCancel.displayName = "AlertDialogCancel"

export {
  AlertDialog,
  AlertDialogContent,
  AlertDialogHeader,
  AlertDialogFooter,
  AlertDialogTitle,
  AlertDialogDescription,
  AlertDialogAction,
  AlertDialogCancel,
}

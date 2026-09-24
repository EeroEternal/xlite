import * as React from "react"
import { createPortal } from "react-dom"
import { DialogPortalContext } from "@/components/ui/dialog-portal-context"
import { cn } from "@/lib/utils"

interface DialogProps {
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

/** Prefer form fields for initial focus so header close buttons do not steal it. */
const INITIAL_FOCUS_SELECTOR = [
  'input:not([disabled]):not([type="hidden"]):not([type="button"]):not([type="submit"]):not([type="checkbox"]):not([type="radio"]):not([type="file"])',
  "textarea:not([disabled])",
  "select:not([disabled])",
  '[role="combobox"]:not([disabled])',
].join(",")

function isVisibleFocusTarget(el: HTMLElement) {
  return !el.hasAttribute("disabled") && el.getAttribute("aria-hidden") !== "true"
}

function getFocusable(container: HTMLElement) {
  return Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)).filter(
    isVisibleFocusTarget
  )
}

function getInitialFocus(container: HTMLElement): HTMLElement | null {
  const field = Array.from(
    container.querySelectorAll<HTMLElement>(INITIAL_FOCUS_SELECTOR)
  ).find(isVisibleFocusTarget)
  if (field) return field
  return getFocusable(container)[0] ?? null
}

type DialogContentContextValue = {
  setContentNode: (node: HTMLDivElement | null) => void
}

const DialogContentContext = React.createContext<DialogContentContextValue | null>(null)

const Dialog = ({ open, onOpenChange, children }: DialogProps) => {
  const contentRef = React.useRef<HTMLDivElement | null>(null)
  const backdropRef = React.useRef<HTMLDivElement | null>(null)
  const previouslyFocusedRef = React.useRef<HTMLElement | null>(null)
  const onOpenChangeRef = React.useRef(onOpenChange)
  const setContentNode = React.useCallback((node: HTMLDivElement | null) => {
    contentRef.current = node
  }, [])

  React.useEffect(() => {
    onOpenChangeRef.current = onOpenChange
  }, [onOpenChange])

  React.useEffect(() => {
    if (open) {
      document.body.style.overflow = "hidden"
      document.body.style.paddingRight = "0px"
    } else {
      document.body.style.overflow = ""
      document.body.style.paddingRight = ""
    }
    return () => {
      document.body.style.overflow = ""
      document.body.style.paddingRight = ""
    }
  }, [open])

  // Only react to open/close. Do not depend on onOpenChange identity — parent
  // re-renders while typing would otherwise re-run focusFirst and steal focus.
  React.useEffect(() => {
    if (!open) {
      previouslyFocusedRef.current?.focus?.()
      return
    }

    previouslyFocusedRef.current = document.activeElement as HTMLElement | null

    const focusFirst = () => {
      const root = contentRef.current
      if (!root) return
      ;(getInitialFocus(root) ?? root).focus({ preventScroll: true })
    }
    const rafId = window.requestAnimationFrame(focusFirst)

    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault()
        e.stopPropagation()
        onOpenChangeRef.current?.(false)
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

    const handleBackdropPointerDown = (event: PointerEvent) => {
      const backdrop = backdropRef.current
      if (!backdrop || event.target !== backdrop) return
      event.preventDefault()
      event.stopPropagation()
      onOpenChangeRef.current?.(false)
    }
    document.addEventListener("pointerdown", handleBackdropPointerDown, true)

    return () => {
      window.cancelAnimationFrame(rafId)
      document.removeEventListener("keydown", handleKeyDown, true)
      document.removeEventListener("pointerdown", handleBackdropPointerDown, true)
    }
  }, [open])

  if (!open) return null

  return createPortal(
    <DialogContentContext.Provider value={{ setContentNode }}>
      <div className="fixed inset-0 z-[100] flex items-center justify-center p-4">
        <div
          ref={backdropRef}
          data-dialog-backdrop=""
          className="fixed inset-0 bg-black/50 backdrop-blur-sm transition-all"
          aria-hidden="true"
        />
        <div className="relative z-[101] flex max-h-full w-full max-w-full items-center justify-center pointer-events-none">
          <div
            className="pointer-events-auto flex max-h-full w-full max-w-full justify-center"
            onClick={(e) => e.stopPropagation()}
          >
            {children}
          </div>
        </div>
      </div>
    </DialogContentContext.Provider>,
    document.body
  )
}

const DialogContent = React.forwardRef<HTMLDivElement, React.HTMLAttributes<HTMLDivElement>>(
  ({ className, children, ...props }, ref) => {
    const ctx = React.useContext(DialogContentContext)
    const [portalContainer, setPortalContainer] = React.useState<HTMLDivElement | null>(null)
    const setRefs = React.useCallback(
      (node: HTMLDivElement | null) => {
        setPortalContainer(node)
        ctx?.setContentNode(node)
        if (typeof ref === "function") ref(node)
        else if (ref) ref.current = node
      },
      [ctx, ref]
    )

    return (
      <DialogPortalContext.Provider value={portalContainer}>
        <div
          ref={setRefs}
          role="dialog"
          aria-modal="true"
          tabIndex={-1}
          className={cn(
            // Viewport bounds (design.md hard rule 5): every dialog is capped at
            // 85vh with an internal vertical scrollbar by default. Callers may
            // override max-h; flex-column layouts pass overflow-hidden and
            // scroll inside a dedicated body region instead.
            "relative max-h-[85vh] w-full max-w-2xl overflow-y-auto overflow-x-hidden scrollbar-hover rounded-lg border bg-background p-6 shadow-lg outline-none",
            className
          )}
          {...props}
        >
          {children}
        </div>
      </DialogPortalContext.Provider>
    )
  }
)
DialogContent.displayName = "DialogContent"

const DialogHeader = ({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) => (
  <div className={cn("mb-4 flex flex-col space-y-1.5 text-center sm:text-left", className)} {...props} />
)
DialogHeader.displayName = "DialogHeader"

const DialogFooter = ({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) => (
  <div
    className={cn("mt-4 flex flex-col-reverse sm:flex-row sm:justify-end sm:space-x-2", className)}
    {...props}
  />
)
DialogFooter.displayName = "DialogFooter"

const DialogTitle = React.forwardRef<HTMLHeadingElement, React.HTMLAttributes<HTMLHeadingElement>>(
  ({ className, ...props }, ref) => (
    <h2 ref={ref} className={cn("text-lg font-semibold leading-none tracking-tight", className)} {...props} />
  )
)
DialogTitle.displayName = "DialogTitle"

const DialogDescription = React.forwardRef<
  HTMLParagraphElement,
  React.HTMLAttributes<HTMLParagraphElement>
>(({ className, ...props }, ref) => (
  <p ref={ref} className={cn("text-sm text-muted-foreground", className)} {...props} />
))
DialogDescription.displayName = "DialogDescription"

export { Dialog, DialogContent, DialogHeader, DialogFooter, DialogTitle, DialogDescription }

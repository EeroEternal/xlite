import * as React from "react"

export const DialogPortalContext = React.createContext<HTMLElement | null>(null)

export function useDialogPortalContainer() {
  return React.useContext(DialogPortalContext)
}

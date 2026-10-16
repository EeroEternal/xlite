import { createContext, useContext } from "react"

export type ThemePreference = "light" | "dark" | "system"

export interface ThemeContextValue {
  preference: ThemePreference
  resolvedTheme: "light" | "dark"
  setPreference: (preference: ThemePreference) => void
}

export const ThemeContext = createContext<ThemeContextValue | null>(null)

export function useTheme() {
  const context = useContext(ThemeContext)
  if (!context) throw new Error("useTheme must be used within ThemeProvider")
  return context
}

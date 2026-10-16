import { useEffect, useMemo, useState } from "react"

import { ThemeContext, type ThemeContextValue, type ThemePreference } from "@/lib/theme-context"

function getStoredPreference(): ThemePreference {
  const stored = localStorage.getItem("theme")
  return stored === "light" || stored === "dark" || stored === "system" ? stored : "system"
}

function resolveTheme(preference: ThemePreference) {
  if (preference !== "system") return preference
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"
}

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const [preference, setPreferenceState] = useState<ThemePreference>(getStoredPreference)
  const [resolvedTheme, setResolvedTheme] = useState<"light" | "dark">(() => resolveTheme(preference))

  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)")
    const applyTheme = () => {
      const nextTheme = resolveTheme(preference)
      document.documentElement.classList.toggle("dark", nextTheme === "dark")
      document.documentElement.style.colorScheme = nextTheme
      setResolvedTheme(nextTheme)
    }

    applyTheme()
    media.addEventListener("change", applyTheme)
    return () => media.removeEventListener("change", applyTheme)
  }, [preference])

  const value = useMemo<ThemeContextValue>(() => ({
    preference,
    resolvedTheme,
    setPreference: (nextPreference) => {
      localStorage.setItem("theme", nextPreference)
      setPreferenceState(nextPreference)
    },
  }), [preference, resolvedTheme])

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>
}

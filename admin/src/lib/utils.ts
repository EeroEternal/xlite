import { type ClassValue, clsx } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

export function getLocaleTag(locale: string) {
  if (locale === "zh") return "zh-CN"
  if (locale === "en") return "en-CA"
  return locale
}

export function formatDateForLocale(dateString: string, locale: string) {
  if (!dateString) return ""
  return new Date(dateString).toLocaleDateString(getLocaleTag(locale))
}

export function formatNumber(value: number, locale: string) {
  return value.toLocaleString(getLocaleTag(locale))
}

export function formatDateTime(
  dateString?: string | null,
  locale?: string,
  short: boolean = false
): string {
  if (!dateString) return ""
  const date = new Date(dateString)
  if (Number.isNaN(date.getTime())) return String(dateString)
  const localeTag = getLocaleTag(locale || "en")

  if (short) {
    return new Intl.DateTimeFormat(localeTag, {
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hour12: false,
    }).format(date)
  }

  if (locale === "zh") {
    const datePart = `${date.getFullYear()}年${date.getMonth() + 1}月${date.getDate()}日`
    const timePart = new Intl.DateTimeFormat("zh-CN", {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    }).format(date)
    return `${datePart} ${timePart}`
  }
  return new Intl.DateTimeFormat(localeTag, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).format(date)
}

export function formatDuration(ms?: number | null): string {
  const w = ms ?? 0
  if (w <= 0) return "0s"
  if (w < 1000) return `${Math.round(w)}ms`
  const totalSeconds = Math.floor(w / 1000)
  if (totalSeconds < 60) return `${totalSeconds}s`
  const minutes = Math.floor(totalSeconds / 60)
  const seconds = totalSeconds % 60
  return `${minutes}m ${seconds}s`
}

export const formatWait = formatDuration

export function formatCreatedAt(dateString?: string | null, language?: string): string {
  if (!dateString) return "—"
  const date = new Date(dateString)
  if (Number.isNaN(date.getTime())) return "—"
  if (language === "zh") {
    const y = date.getFullYear()
    const m = date.getMonth() + 1
    const d = date.getDate()
    const hh = String(date.getHours()).padStart(2, "0")
    const mm = String(date.getMinutes()).padStart(2, "0")
    return `${y}.${m}.${d} ${hh}:${mm}`
  }
  return date.toLocaleString("en-CA", {
    year: "numeric",
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  })
}

export function formatJsonString(str: string | null | undefined): string {
  if (!str) return ""
  try {
    const parsed = JSON.parse(str)
    return JSON.stringify(parsed, null, 2)
  } catch {
    return str
  }
}

/**
 * Formats error messages to be more user-friendly, stripping JSON if possible.
 */
export function formatError(error: string | null | undefined): string {
  if (!error) return ""

  // If it's a "Provider returned error..." string, try to extract the JSON part
  const jsonMatch = error.match(/^(?:Provider returned error|Connection failed|API error):?.*?:?\s*(\{.*\})$/is)
  const contentToParse = jsonMatch ? jsonMatch[1] : error

  try {
    const parsed = JSON.parse(contentToParse)
    
    // Check common error structures
    const message = 
      parsed.error?.message || 
      parsed.error || 
      parsed.message || 
      parsed.msg ||
      (typeof parsed === 'string' ? parsed : null)

    if (message && typeof message === 'string') {
      // If we extracted a message from a "Provider returned error" string, 
      // we might want to keep the status code if it was present
      const prefixMatch = error.match(/^(Provider returned error \d+ [^:]+):/)
      if (prefixMatch) {
        return `${prefixMatch[1]}: ${message}`
      }
      return message
    }
  } catch {
    // If not JSON, just return the original or cleaned up string
  }

  // Clean up common prefixes if they don't add much value
  return error
    .replace(/^Provider returned error \d+ [^:]+: (\{.*\})$/is, '$1') // If it was just JSON and failed to parse somehow
    .trim()
}

export async function copyTextToClipboard(text: string) {
  if (typeof navigator !== "undefined" && window.isSecureContext && navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text)
    return
  }

  if (typeof document === "undefined") {
    throw new Error("Clipboard API is unavailable")
  }

  const textArea = document.createElement("textarea")
  textArea.value = text
  textArea.setAttribute("readonly", "")
  textArea.style.position = "fixed"
  textArea.style.top = "0"
  textArea.style.left = "0"
  textArea.style.opacity = "0"
  textArea.style.pointerEvents = "none"

  document.body.appendChild(textArea)
  textArea.focus()
  textArea.select()
  textArea.setSelectionRange(0, text.length)

  const copied = document.execCommand("copy")
  document.body.removeChild(textArea)

  if (!copied) {
    throw new Error("Copy command failed")
  }
}

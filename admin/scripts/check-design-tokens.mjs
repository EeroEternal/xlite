#!/usr/bin/env node
/**
 * Design-token guard for the Admin UI.
 *
 * Fails when product code (admin/src, excluding i18n locale bundles) reaches for
 * a native Tailwind palette color, a hard-coded color, a non-token radius, an
 * `!important` spacing override, or mixes card-surface tokens incorrectly,
 * instead of using the semantic tokens documented in docs/design/tokens.md and
 * defined in admin/src/index.css.
 *
 * Rationale: docs/design.md Hard rule 1. The same drift was previously fixed by
 * hand across ~15 files; this script keeps it from coming back. Code-highlight
 * themes are out of scope (see design.md Scope).
 *
 * Surface model (docs/design/surfaces.md):
 *   level 1 (on the canvas)  -> bg-card + rounded-* + shadow-card, no hairline
 *   level 2 (inside a level 1 surface or dialog) -> tonal fill (bg-muted/*), no hairline
 *
 * Run via `npm run lint` (or `npm run lint:tokens`).
 */
import { readdirSync, readFileSync, statSync } from "node:fs"
import { join, relative } from "node:path"
import { fileURLToPath } from "node:url"

const ADMIN_ROOT = fileURLToPath(new URL("..", import.meta.url))
const SRC_ROOT = join(ADMIN_ROOT, "src")
const SCANNED_EXTENSIONS = [".ts", ".tsx"]
/** Locale bundles embed changelog prose that legitimately quotes brand hex values. */
const EXCLUDED_DIRECTORIES = [join(SRC_ROOT, "lib", "i18n", "locales")]
/**
 * Marketing surfaces are allowed the heavier weights; docs/design/typography.md
 * caps product UI at 400/500/600.
 */
const MARKETING_FILES = [join(SRC_ROOT, "pages", "landing.tsx"), join(SRC_ROOT, "pages", "login.tsx")]

const PALETTE =
  "(?:slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose)"
const COLOR_UTILITIES =
  "(?:bg|text|border|ring|outline|divide|from|via|to|fill|stroke|decoration|placeholder|caret|accent|shadow)"

/** Strips leading variants (`hover:`, `dark:`, `sm:`, `[&_svg]:`) from a class token. */
const stripVariants = (token) => token.replace(/^(?:[^:\s]*:)+/, "")

const classTokens = (line) =>
  line
    .split(/[\s"'`{}()[\],;/]+/)
    .map((token) => stripVariants(token.trim()))
    .filter(Boolean)

const hasBgCard = (tokens) => tokens.some((token) => /^bg-card(\/\d+)?$/.test(token))
const NEUTRAL_BORDER = /^(border|border-border(\/\d+)?)$/
/** Status-tinted borders are a documented exception: they carry meaning, not just separation. */
const SEMANTIC_BORDER = /^border-(?:primary|success|warning|destructive|info|inactive|experimental)(\/\d+)?$/
const DEFAULT_SHADOW = /^shadow-(?:sm|md|lg|xl|2xl)$/

/**
 * Radius scale (docs/design/surfaces.md Shapes): 4/6/8/full, optionally directional
 * (`rounded-t-lg`). Anything else is the Invented radius anti-pattern — including arbitrary
 * values (`rounded-[10px]`) and directional out-of-scale variants (`rounded-r-xl`), which a
 * blacklist of literal class names could not see.
 */
const RADIUS_ALLOWED =
  /^rounded(?:-(?:t|r|b|l|tl|tr|br|bl))?-(?:sm|md|lg|full|none)$|^rounded(?:-(?:t|r|b|l|tl|tr|br|bl))?$/
const RADIUS_CANDIDATE = /\brounded(?:-[a-z0-9]+)*(?:-\[[^\]]+\])?/gi

const RULES = [
  {
    id: "native-palette",
    hint: "use semantic tokens: primary / secondary / success / warning / destructive / info / muted / inactive / experimental",
    detect: (line) => new RegExp(`\\b${COLOR_UTILITIES}-${PALETTE}-[0-9]{2,3}\\b`).exec(line)?.[0] ?? null,
  },
  {
    id: "hard-coded-color",
    hint: "use tokens from admin/src/index.css, e.g. hsl(var(--chart-1)) for SVG series or bg-info/10",
    detect: (line) => /#[0-9a-fA-F]{3,8}\b|rgba?\(/.exec(line)?.[0] ?? null,
  },
  {
    id: "invented-radius",
    hint: "the radius scale is 4/6/8/full: rounded-sm (4px) / rounded-md (6px) / rounded-lg (8px) / rounded-full; rounded-xl (12px), rounded-2xl/3xl and arbitrary values like rounded-[10px] are rejected",
    detect: (line) => {
      for (const candidate of line.match(RADIUS_CANDIDATE) ?? []) {
        if (!RADIUS_ALLOWED.test(candidate)) return candidate
      }
      return null
    },
  },
  {
    id: "important-spacing-override",
    hint: "adjust PageShell / PageContainer / the shared primitive instead of forcing !important spacing",
    detect: (line) => /!(?:p|px|py|pt|pb|pl|pr|m|mx|my|mt|mb|ml|mr)-/.exec(line)?.[0] ?? null,
  },
  {
    id: "invented-weight",
    hint: "product UI weights are 400/500/600; use font-semibold instead of font-bold (marketing surfaces are exempt)",
    detect: (line) => (/\bfont-bold\b/.exec(line)?.[0] ?? null),
  },
  {
    id: "card-surface-hairline",
    hint: "level-1 card surfaces float on the canvas (shadow-card, no border); level-2 surfaces use a tonal fill (bg-muted/*) instead of a hairline",
    detect: (line, tokens) => {
      if (!hasBgCard(tokens) || tokens.some((token) => SEMANTIC_BORDER.test(token))) return null
      return tokens.find((token) => NEUTRAL_BORDER.test(token)) ?? null
    },
  },
  {
    id: "card-surface-shadow",
    hint: "card surfaces use the tokenized elevation shadow-card / hover:shadow-card-hover",
    detect: (line, tokens) =>
      hasBgCard(tokens) ? tokens.find((token) => DEFAULT_SHADOW.test(token)) ?? null : null,
  },
]

function collectFiles(directory) {
  const files = []
  for (const entry of readdirSync(directory)) {
    const fullPath = join(directory, entry)
    if (EXCLUDED_DIRECTORIES.some((excluded) => fullPath.startsWith(excluded))) continue
    if (statSync(fullPath).isDirectory()) {
      files.push(...collectFiles(fullPath))
      continue
    }
    if (SCANNED_EXTENSIONS.includes(fullPath.slice(fullPath.lastIndexOf(".")))) files.push(fullPath)
  }
  return files
}

const findings = []
for (const file of collectFiles(SRC_ROOT)) {
  const isMarketing = MARKETING_FILES.includes(file)
  const lines = readFileSync(file, "utf8").split("\n")
  lines.forEach((line, index) => {
    const tokens = classTokens(line)
    for (const rule of RULES) {
      if (rule.id === "invented-weight" && isMarketing) continue
      const match = rule.detect(line, tokens)
      if (!match) continue
      findings.push({
        file: relative(ADMIN_ROOT, file),
        line: index + 1,
        rule: rule.id,
        match,
        hint: rule.hint,
      })
    }
  })
}

if (findings.length === 0) {
  console.log("design-tokens: OK (no hard-coded colors, invented radii, !important spacing, or surface drift)")
  process.exit(0)
}

console.error(`design-tokens: ${findings.length} violation(s) — see docs/design.md Hard rule 1`)
for (const finding of findings) {
  console.error(`  ${finding.file}:${finding.line}  [${finding.rule}] ${finding.match}`)
  console.error(`    → ${finding.hint}`)
}
process.exit(1)

import { useState, type FormEvent } from "react"
import { useNavigate } from "react-router-dom"
import { Eye, EyeOff } from "lucide-react"
import { LanguageSwitcher } from "@/components/language-switcher"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { useAuth } from "@/lib/auth"
import { t } from "@/lib/i18n"
import { login as loginRequest } from "@/lib/resources/auth"

export function LoginPage() {
  const [username, setUsername] = useState("admin")
  const [password, setPassword] = useState("")
  const [showPassword, setShowPassword] = useState(false)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState("")
  const navigate = useNavigate()
  const { login } = useAuth()

  const handleSubmit = async (event: FormEvent) => {
    event.preventDefault()
    setError("")
    setLoading(true)
    try {
      const data = await loginRequest(username, password)
      if (data.success && data.data?.user) {
        login(data.data.user)
        navigate("/")
      } else {
        setError(
          data.message === "Invalid username or password"
            ? t("auth.invalidCredentials")
            : data.message || t("auth.loginFailed"),
        )
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : ""
      setError(
        message === "Invalid username or password" ? t("auth.invalidCredentials") : message || t("common.networkError"),
      )
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className="flex min-h-screen items-center justify-center bg-canvas p-6">
      <div className="absolute right-4 top-4">
        <LanguageSwitcher />
      </div>
      <form
        onSubmit={(event) => void handleSubmit(event)}
        className="w-full max-w-sm rounded-lg bg-card p-6 shadow-card"
      >
        <div className="mb-6 flex items-center gap-3">
          <img src="/favicon.svg" alt="xlite" className="h-8 w-8" />
          <h1 className="text-page-title text-foreground">{t("auth.title")}</h1>
        </div>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="username">{t("auth.username")}</Label>
            <Input
              id="username"
              value={username}
              onChange={(event) => setUsername(event.target.value)}
              autoComplete="username"
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="password">{t("auth.password")}</Label>
            <div className="relative">
              <Input
                id="password"
                type={showPassword ? "text" : "password"}
                value={password}
                onChange={(event) => setPassword(event.target.value)}
                autoComplete="current-password"
                className="pr-10"
              />
              <Button
                type="button"
                variant="ghost"
                size="icon-sm"
                className="absolute right-1 top-1/2 -translate-y-1/2"
                aria-label={t("auth.password")}
                onClick={() => setShowPassword((open) => !open)}
              >
                {showPassword ? <EyeOff aria-hidden="true" /> : <Eye aria-hidden="true" />}
              </Button>
            </div>
          </div>
          {error ? <p className="text-sm text-destructive">{error}</p> : null}
          <Button type="submit" className="w-full" disabled={loading}>
            {t("auth.submit")}
          </Button>
        </div>
      </form>
    </div>
  )
}

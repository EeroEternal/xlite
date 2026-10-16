import { LogOut } from "lucide-react"
import { useNavigate } from "react-router-dom"
import { LanguageSwitcher } from "@/components/language-switcher"
import { ThemeSwitcher } from "@/components/theme-switcher"
import { Button } from "@/components/ui/button"
import { Separator } from "@/components/ui/separator"
import { SidebarTrigger } from "@/components/ui/sidebar"
import { t } from "@/lib/i18n"
import { useAuth } from "@/lib/auth"
import { logout as logoutRequest } from "@/lib/resources/auth"

export function SiteHeader() {
  const navigate = useNavigate()
  const { logout } = useAuth()

  const handleLogout = async () => {
    try {
      await logoutRequest()
    } catch {
      // Session is cleared locally either way.
    }
    logout()
    navigate("/login")
  }

  return (
    <header className="relative z-50 flex h-14 shrink-0 items-center justify-between bg-background/95 px-4 backdrop-blur supports-[backdrop-filter]:bg-background/60">
      <div className="flex items-center gap-3">
        <SidebarTrigger className="-ml-1" />
      </div>
      <div className="flex items-center gap-2">
        <ThemeSwitcher />
        <LanguageSwitcher />
        <Separator orientation="vertical" className="mx-1 hidden h-4 sm:block" />
        <Button
          variant="ghost"
          size="icon-sm"
          aria-label={t("auth.logout")}
          onClick={() => void handleLogout()}
        >
          <LogOut aria-hidden="true" />
        </Button>
      </div>
    </header>
  )
}

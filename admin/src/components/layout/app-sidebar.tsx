import type { ComponentProps } from "react"
import { Link, useLocation } from "react-router-dom"
import { KeyRound, LayoutDashboard, Server, Settings, Waypoints } from "lucide-react"
import {
  Sidebar,
  SidebarContent,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarRail,
} from "@/components/ui/sidebar"
import { t } from "@/lib/i18n"
import { cn } from "@/lib/utils"

const items = [
  { href: "/", icon: LayoutDashboard, key: "nav.overview" },
  { href: "/providers", icon: Server, key: "nav.providers" },
  { href: "/models", icon: Waypoints, key: "nav.models" },
  { href: "/api-keys", icon: KeyRound, key: "nav.apiKeys" },
  { href: "/settings", icon: Settings, key: "nav.settings" },
] as const

export function AppSidebar(props: ComponentProps<typeof Sidebar>) {
  const location = useLocation()

  return (
    <Sidebar collapsible="icon" {...props}>
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton size="lg" asChild>
              <Link to="/">
                <div className="flex size-8 items-center justify-center">
                  <img src="/favicon.svg" alt="xlite" className="size-8" />
                </div>
                <span className="text-base font-semibold">xlite</span>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>
      <SidebarContent>
        <SidebarMenu className="px-2">
          {items.map((item) => {
            const isActive = location.pathname === item.href
            return (
              <SidebarMenuItem key={item.href}>
                <SidebarMenuButton asChild tooltip={t(item.key)} isActive={isActive}>
                  <Link to={item.href} className={cn("flex items-center gap-3")}>
                    <item.icon className="size-4" />
                    <span>{t(item.key)}</span>
                  </Link>
                </SidebarMenuButton>
              </SidebarMenuItem>
            )
          })}
        </SidebarMenu>
      </SidebarContent>
      <SidebarRail />
    </Sidebar>
  )
}

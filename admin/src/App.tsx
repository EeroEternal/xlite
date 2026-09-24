import { Suspense } from "react"
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom"
import { DashboardLayout } from "@/components/layout/dashboard-layout"
import { PageLoading } from "@/components/ui/page-loading"
import { TooltipProvider } from "@/components/ui/tooltip"
import { AuthProvider, useAuth } from "@/lib/auth"
import { ThemeProvider } from "@/lib/theme"
import { ApiKeysPage } from "@/pages/api-keys"
import { LoginPage } from "@/pages/login"
import { ModelsPage } from "@/pages/models"
import { OverviewPage } from "@/pages/overview"
import { ProvidersPage } from "@/pages/providers"
import { SettingsPage } from "@/pages/settings"

function GuardedLayout() {
  const { isAuthenticated, loading } = useAuth()
  if (loading) return <PageLoading fullScreen />
  if (!isAuthenticated) return <Navigate to="/login" replace />
  return <DashboardLayout />
}

function LoginRoute() {
  const { isAuthenticated, loading } = useAuth()
  if (loading) return <PageLoading fullScreen />
  if (isAuthenticated) return <Navigate to="/" replace />
  return <LoginPage />
}

export default function App() {
  return (
    <ThemeProvider>
      <TooltipProvider>
        <AuthProvider>
          <BrowserRouter>
            <Suspense fallback={<PageLoading fullScreen />}>
              <Routes>
                <Route path="/login" element={<LoginRoute />} />
                <Route path="/" element={<GuardedLayout />}>
                  <Route index element={<OverviewPage />} />
                  <Route path="providers" element={<ProvidersPage />} />
                  <Route path="models" element={<ModelsPage />} />
                  <Route path="api-keys" element={<ApiKeysPage />} />
                  <Route path="settings" element={<SettingsPage />} />
                </Route>
                <Route path="*" element={<Navigate to="/" replace />} />
              </Routes>
            </Suspense>
          </BrowserRouter>
        </AuthProvider>
      </TooltipProvider>
    </ThemeProvider>
  )
}

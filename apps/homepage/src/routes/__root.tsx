import { createRootRoute, Outlet } from '@tanstack/react-router'
import { Atmosphere } from '@/components/site/atmosphere'
import { I18nProvider } from '@/lib/i18n'
import '@/styles.css'

export const Route = createRootRoute({
  component: RootDocument,
})

function RootDocument() {
  return (
    <I18nProvider>
      <Atmosphere />
      <Outlet />
    </I18nProvider>
  )
}

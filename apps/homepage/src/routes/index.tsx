import { createFileRoute } from '@tanstack/react-router'
import { useEffect } from 'react'
import { SiteFooter } from '@/components/site/footer'
import { Landing } from '@/components/site/landing'
import { SiteNav } from '@/components/site/nav'
import { useI18n } from '@/lib/i18n'

export const Route = createFileRoute('/')({ component: Home })

function Home() {
  const { t } = useI18n()

  useEffect(() => {
    document.title = t.metaTitle
  }, [t.metaTitle])

  return (
    <div className="relative">
      <SiteNav />
      <main>
        <Landing />
      </main>
      <SiteFooter />
    </div>
  )
}

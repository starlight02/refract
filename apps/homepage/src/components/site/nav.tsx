import { Link } from '@tanstack/react-router'
import { Menu, X } from 'lucide-react'
import { useState } from 'react'
import { signOut } from '@/lib/auth/client'
import { useCurrentUserState } from '@/lib/auth/use-current-user'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Wordmark } from './logo'

const GITHUB = 'https://github.com/starlight02/refract'

const LINKS = [
  { href: '#kinds', key: 'protocols' },
  { href: '#why', key: 'why' },
  { href: '#features', key: 'features' },
  { href: '#architecture', key: 'architecture' },
] as const

export function SiteNav() {
  const { t, locale, setLocale } = useI18n()
  const [open, setOpen] = useState(false)

  return (
    <header className="sticky top-0 z-40">
      <div className="absolute inset-0 bg-bg/75 backdrop-blur-md" />
      <div className="relative mx-auto flex h-16 max-w-6xl items-center justify-between gap-4 px-5">
        <a href="#top" className="relative shrink-0 text-fg">
          <Wordmark />
        </a>

        <nav className="absolute left-1/2 hidden -translate-x-1/2 items-center gap-7 lg:flex">
          {LINKS.map((l) => (
            <a
              key={l.href}
              href={l.href}
              className="text-sm text-muted transition-colors duration-150 hover:text-fg"
            >
              {t.nav[l.key]}
            </a>
          ))}
        </nav>

        <div className="relative flex items-center gap-1.5 sm:gap-2">
          <LangSwitch locale={locale} setLocale={setLocale} />
          <a
            href={GITHUB}
            target="_blank"
            rel="noreferrer"
            className="hidden h-9 items-center rounded-full px-3 text-sm text-muted transition-colors duration-150 hover:text-fg sm:inline-flex"
          >
            GitHub
          </a>
          <AuthSlot signIn={t.nav.signIn} signOutLabel={t.nav.signOut} />
          <button
            type="button"
            className="inline-flex size-11 items-center justify-center rounded-full text-fg lg:hidden"
            aria-expanded={open}
            aria-label={open ? t.nav.close : t.nav.menu}
            onClick={() => setOpen((v) => !v)}
          >
            {open ? <X className="size-5" /> : <Menu className="size-5" />}
          </button>
        </div>
      </div>

      {open ? (
        <div className="relative border-t border-border bg-bg lg:hidden">
          <nav className="mx-auto flex max-w-6xl flex-col px-5 py-3">
            {LINKS.map((l) => (
              <a
                key={l.href}
                href={l.href}
                onClick={() => setOpen(false)}
                className="flex min-h-11 items-center text-sm text-fg"
              >
                {t.nav[l.key]}
              </a>
            ))}
            <a
              href={GITHUB}
              target="_blank"
              rel="noreferrer"
              className="flex min-h-11 items-center text-sm text-muted sm:hidden"
            >
              GitHub
            </a>
          </nav>
        </div>
      ) : null}
    </header>
  )
}

function LangSwitch({
  locale,
  setLocale,
}: {
  locale: 'zh' | 'en'
  setLocale: (l: 'zh' | 'en') => void
}) {
  return (
    <div className="flex items-center font-mono text-[0.7rem] tracking-wider">
      <button
        type="button"
        onClick={() => setLocale('en')}
        className={cn(
          'h-9 px-2 transition-colors duration-150',
          locale === 'en' ? 'text-fg' : 'text-subtle hover:text-muted',
        )}
      >
        EN
      </button>
      <span className="text-subtle">/</span>
      <button
        type="button"
        onClick={() => setLocale('zh')}
        className={cn(
          'h-9 px-2 transition-colors duration-150',
          locale === 'zh' ? 'text-fg' : 'text-subtle hover:text-muted',
        )}
      >
        中文
      </button>
    </div>
  )
}

function AuthSlot({ signIn, signOutLabel }: { signIn: string; signOutLabel: string }) {
  const { user, isPending } = useCurrentUserState()

  if (isPending) {
    return <div className="h-9 w-16 rounded-full bg-fg/8" aria-hidden="true" />
  }

  if (user) {
    const label = user.displayName ?? user.primaryEmail ?? 'Account'
    return (
      <div className="flex items-center gap-2">
        {user.profileImageUrl ? (
          <img src={user.profileImageUrl} alt="" className="size-8 rounded-full object-cover" />
        ) : (
          <span className="grid size-8 place-items-center rounded-full bg-fg/10 font-mono text-xs">
            {label.charAt(0).toUpperCase()}
          </span>
        )}
        <button
          type="button"
          onClick={() => void signOut()}
          className="hidden h-9 text-sm text-muted transition-colors hover:text-fg sm:inline"
        >
          {signOutLabel}
        </button>
      </div>
    )
  }

  return (
    <Button asChild variant="outline" size="sm">
      <Link to="/login">{signIn}</Link>
    </Button>
  )
}

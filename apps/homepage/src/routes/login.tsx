import { createFileRoute, Link } from '@tanstack/react-router'
import { GROK_PROVIDERS, authEnabled, signIn } from '@/lib/auth/client'
import { Button } from '@/components/ui/button'
import { Wordmark } from '@/components/site/logo'
import { useI18n } from '@/lib/i18n'

export const Route = createFileRoute('/login')({ component: Login })

function Login() {
  const { t, locale, setLocale } = useI18n()

  return (
    <main className="relative flex min-h-dvh flex-col">
      <header className="mx-auto flex h-16 w-full max-w-6xl items-center justify-between px-5">
        <Link to="/" className="text-fg">
          <Wordmark />
        </Link>
        <div className="flex items-center font-mono text-[0.7rem] tracking-wider">
          <button
            type="button"
            onClick={() => setLocale('en')}
            className={locale === 'en' ? 'h-9 px-2 text-fg' : 'h-9 px-2 text-subtle'}
          >
            EN
          </button>
          <span className="text-subtle">/</span>
          <button
            type="button"
            onClick={() => setLocale('zh')}
            className={locale === 'zh' ? 'h-9 px-2 text-fg' : 'h-9 px-2 text-subtle'}
          >
            中文
          </button>
        </div>
      </header>

      <div className="flex flex-1 items-center justify-center px-5 py-16">
        <div className="w-full max-w-sm">
          <p className="font-mono text-xs tracking-[0.2em] text-subtle uppercase">
            {t.login.kicker}
          </p>
          <h1 className="font-display mt-3 text-3xl tracking-[-0.03em]">{t.login.title}</h1>
          <p className="mt-3 text-sm leading-relaxed text-muted">{t.login.lede}</p>

          <div className="mt-8 space-y-3">
            {authEnabled ? (
              GROK_PROVIDERS.map((p) => (
                <Button
                  key={p.providerId}
                  type="button"
                  variant="outline"
                  size="lg"
                  className="w-full rounded-lg"
                  onClick={() => signIn(p.providerId, { callbackURL: '/' })}
                >
                  {p.idp === 'google' ? <GoogleMark /> : <XMark />}
                  {t.login.continueWith} {p.label}
                </Button>
              ))
            ) : (
              <p className="text-sm text-muted">{t.login.disabled}</p>
            )}
          </div>

          <Link
            to="/"
            className="mt-10 inline-flex min-h-11 items-center text-sm text-muted transition-colors hover:text-fg"
          >
            {t.login.back}
          </Link>
        </div>
      </div>
    </main>
  )
}

function GoogleMark() {
  return (
    <svg viewBox="0 0 24 24" className="size-4" aria-hidden="true">
      <path
        fill="currentColor"
        d="M21.6 12.23c0-.74-.07-1.45-.19-2.13H12v4.03h5.38a4.6 4.6 0 0 1-2 3.02v2.5h3.24c1.9-1.75 2.98-4.33 2.98-7.42Z"
        opacity="0.95"
      />
      <path
        fill="currentColor"
        d="M12 22c2.7 0 4.96-.9 6.62-2.35l-3.24-2.5c-.9.6-2.05.96-3.38.96-2.6 0-4.8-1.76-5.59-4.12H3.06v2.58A10 10 0 0 0 12 22Z"
        opacity="0.75"
      />
      <path
        fill="currentColor"
        d="M6.41 13.99A6.01 6.01 0 0 1 6.1 12c0-.69.12-1.36.31-1.99V7.43H3.06A10 10 0 0 0 2 12c0 1.61.39 3.14 1.06 4.57l3.35-2.58Z"
        opacity="0.6"
      />
      <path
        fill="currentColor"
        d="M12 5.88c1.47 0 2.79.5 3.82 1.5l2.86-2.86C16.95 2.87 14.7 2 12 2A10 10 0 0 0 3.06 7.43l3.35 2.58C7.2 7.64 9.4 5.88 12 5.88Z"
        opacity="0.8"
      />
    </svg>
  )
}

function XMark() {
  return (
    <svg viewBox="0 0 24 24" className="size-4" aria-hidden="true">
      <path
        fill="currentColor"
        d="M13.32 10.9 19.5 3.5h-1.7l-5.2 6.22L8.4 3.5H3.5l6.53 9.72L3.5 20.5h1.7l5.54-6.63 4.4 6.63H20.5l-7.18-9.6Zm-1.96 2.34-.64-.94L5.8 4.66h2.2l4.13 5.9.64.94 5.5 7.86h-2.2l-4.71-6.12Z"
      />
    </svg>
  )
}

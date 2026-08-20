import { useI18n } from '@/lib/i18n'
import { Wordmark } from './logo'

const GITHUB = 'https://github.com/starlight02/refract'

export function SiteFooter() {
  const { t } = useI18n()

  return (
    <footer className="border-t border-border">
      <div className="mx-auto flex max-w-6xl flex-col gap-8 px-5 py-12 sm:flex-row sm:items-end sm:justify-between">
        <div className="max-w-md">
          <Wordmark />
          <p className="mt-4 text-sm leading-relaxed text-muted">{t.footer.line}</p>
        </div>
        <div className="flex flex-wrap gap-x-6 gap-y-3 font-mono text-xs text-muted">
          <a
            className="transition-colors hover:text-fg"
            href={GITHUB}
            target="_blank"
            rel="noreferrer"
          >
            {t.footer.source}
          </a>
          <a
            className="transition-colors hover:text-fg"
            href={`${GITHUB}/blob/master/docs/ARCHITECTURE.md`}
            target="_blank"
            rel="noreferrer"
          >
            {t.footer.architecture}
          </a>
          <a
            className="transition-colors hover:text-fg"
            href={`${GITHUB}/blob/master/docs/OPERATIONS.md`}
            target="_blank"
            rel="noreferrer"
          >
            {t.footer.operations}
          </a>
          <span>{t.footer.license}</span>
        </div>
      </div>
    </footer>
  )
}

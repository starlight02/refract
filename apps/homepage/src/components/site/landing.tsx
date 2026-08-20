import { ArrowUpRight, Check, Copy } from 'lucide-react'
import { useState } from 'react'
import { CURL } from '@/lib/copy'
import { useI18n } from '@/lib/i18n'
import { KINDS, PROTOCOLS, protocolById, type ProtocolId } from '@/lib/protocols'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { PrismStage } from './prism-stage'

const GITHUB = 'https://github.com/starlight02/refract'

export function Landing() {
  const [inbound, setInbound] = useState<ProtocolId>('messages')
  const [outbound, setOutbound] = useState<ProtocolId>('chat')

  return (
    <div id="top">
      <Hero inbound={inbound} outbound={outbound} onInbound={setInbound} onOutbound={setOutbound} />
      <Kinds />
      <Why />
      <Features />
      <Architecture inbound={inbound} outbound={outbound} />
      <Endpoints inbound={inbound} outbound={outbound} onInbound={setInbound} />
    </div>
  )
}

function Hero({
  inbound,
  outbound,
  onInbound,
  onOutbound,
}: {
  inbound: ProtocolId
  outbound: ProtocolId
  onInbound: (id: ProtocolId) => void
  onOutbound: (id: ProtocolId) => void
}) {
  const { t, locale } = useI18n()

  return (
    <section className="relative overflow-hidden">
      <div className="optical-axis pointer-events-none absolute top-[42%] right-0 left-0 h-px" />
      <div className="mx-auto max-w-6xl px-5 pt-16 pb-12 sm:pt-24 sm:pb-16">
        <div className="hero-in max-w-3xl">
          <p className="font-mono text-xs tracking-[0.22em] text-muted uppercase">
            {t.hero.kicker}
          </p>
          <h1 className="font-display mt-6 text-[length:var(--text-display)] leading-[1.08] font-medium tracking-[-0.035em]">
            {t.hero.titleA}
            <br />
            <em
              className={
                locale === 'en'
                  ? 'font-display text-muted not-italic sm:text-fg/90 sm:italic'
                  : 'font-display text-muted not-italic'
              }
            >
              {t.hero.titleB}
            </em>
          </h1>
          <p className="mt-6 max-w-xl text-base leading-relaxed text-muted sm:text-lg">
            {t.hero.lede}
          </p>
          <div className="mt-8 flex flex-wrap items-center gap-3">
            <Button asChild variant="primary" size="lg">
              <a href="#architecture">{t.hero.cta}</a>
            </Button>
            <Button asChild variant="outline" size="lg">
              <a href={GITHUB} target="_blank" rel="noreferrer" className="pr-4">
                {t.hero.secondary}
                <ArrowUpRight className="size-4" />
              </a>
            </Button>
          </div>
          <ul className="mt-10 flex flex-wrap gap-x-5 gap-y-2 font-mono text-[0.72rem] tracking-[0.14em] text-subtle uppercase">
            {t.hero.chips.map((c) => (
              <li key={c}>{c}</li>
            ))}
          </ul>
        </div>

        <div className="mt-16 rounded-xl bg-bg-elevated/80 p-4 shadow-[var(--shadow-border)] sm:p-6 md:p-8">
          <PrismStage
            inbound={inbound}
            outbound={outbound}
            onInbound={onInbound}
            onOutbound={onOutbound}
          />
        </div>
      </div>
    </section>
  )
}

function SectionHead({ kicker, title, lede }: { kicker: string; title: string; lede: string }) {
  return (
    <header className="max-w-2xl">
      <p className="font-mono text-xs tracking-[0.2em] text-subtle uppercase">{kicker}</p>
      <h2 className="font-display mt-3 text-3xl leading-[1.15] font-medium tracking-[-0.03em] sm:text-[2.35rem]">
        {title}
      </h2>
      <p className="mt-4 text-base leading-relaxed text-muted">{lede}</p>
    </header>
  )
}

function Kinds() {
  const { t } = useI18n()
  const items = t.kinds.items

  return (
    <section id="kinds" className="mx-auto max-w-6xl px-5 py-24 sm:py-32">
      <SectionHead kicker={t.kinds.kicker} title={t.kinds.title} lede={t.kinds.lede} />
      <ul className="mt-12 divide-y divide-border border-y border-border">
        {KINDS.map((k) => {
          const item = items[k.id]
          return (
            <li
              key={k.id}
              className="grid grid-cols-1 items-baseline gap-1 py-5 sm:grid-cols-[minmax(0,0.7fr)_minmax(0,1.4fr)_minmax(0,0.9fr)] sm:gap-8"
            >
              <span className="font-display text-xl tracking-[-0.03em]">{item.name}</span>
              <span className="text-sm text-muted">{item.meaning}</span>
              <span className="font-mono text-xs text-subtle">{k.path}</span>
            </li>
          )
        })}
      </ul>
      <blockquote className="mt-12 max-w-3xl">
        <p className="text-sm text-subtle">{t.kinds.quoteLead}</p>
        <p className="font-display mt-3 text-xl leading-snug tracking-[-0.02em] text-fg/90 sm:text-2xl">
          “{t.kinds.quote}”
        </p>
      </blockquote>
    </section>
  )
}

function Why() {
  const { t } = useI18n()

  return (
    <section id="why" className="border-y border-border bg-bg-elevated/40">
      <div className="mx-auto max-w-6xl px-5 py-24 sm:py-32">
        <SectionHead kicker={t.why.kicker} title={t.why.title} lede={t.why.lede} />
        <div className="mt-12 grid gap-px overflow-hidden rounded-xl bg-border shadow-[var(--shadow-border)] md:grid-cols-2">
          <CompareColumn title={t.why.leftTitle} items={t.why.left} tone="muted" />
          <CompareColumn title={t.why.rightTitle} items={t.why.right} tone="fg" />
        </div>
      </div>
    </section>
  )
}

function CompareColumn({
  title,
  items,
  tone,
}: {
  title: string
  items: readonly string[]
  tone: 'muted' | 'fg'
}) {
  return (
    <div className="bg-bg-elevated p-6 sm:p-8">
      <p
        className={cn(
          'font-mono text-xs tracking-[0.18em] uppercase',
          tone === 'fg' ? 'text-primary' : 'text-subtle',
        )}
      >
        {title}
      </p>
      <ul className="mt-6 space-y-4">
        {items.map((item) => (
          <li
            key={item}
            className={cn(
              'border-t border-border pt-4 text-sm leading-relaxed',
              tone === 'fg' ? 'text-fg' : 'text-muted',
            )}
          >
            {item}
          </li>
        ))}
      </ul>
    </div>
  )
}

function Features() {
  const { t } = useI18n()

  return (
    <section id="features" className="mx-auto max-w-6xl px-5 py-24 sm:py-32">
      <div className="grid gap-16 lg:grid-cols-[minmax(0,0.85fr)_minmax(0,1.15fr)]">
        <div className="lg:sticky lg:top-24 lg:self-start">
          <SectionHead kicker={t.features.kicker} title={t.features.title} lede={t.features.lede} />
        </div>
        <ul>
          {t.features.items.map((item) => (
            <li
              key={item.n}
              className="grid grid-cols-[auto_minmax(0,1fr)] gap-5 border-t border-border py-8 first:border-t-0 first:pt-0"
            >
              <span className="font-mono text-xs text-subtle tabular-nums">{item.n}</span>
              <div>
                <h3 className="text-base font-medium tracking-[-0.01em]">{item.title}</h3>
                <p className="mt-2 text-sm leading-relaxed text-muted">{item.body}</p>
              </div>
            </li>
          ))}
        </ul>
      </div>
    </section>
  )
}

function Architecture({ inbound, outbound }: { inbound: ProtocolId; outbound: ProtocolId }) {
  const { t } = useI18n()
  const native = inbound === outbound

  return (
    <section id="architecture" className="border-y border-border bg-bg-elevated/40">
      <div className="mx-auto max-w-6xl px-5 py-24 sm:py-32">
        <SectionHead
          kicker={t.architecture.kicker}
          title={t.architecture.title}
          lede={t.architecture.lede}
        />

        <div className="mt-14 grid items-center gap-8 lg:grid-cols-[1fr_auto_1fr]">
          <SpokeList label={t.architecture.inbound} active={inbound} side="in" />
          <div className="mx-auto flex size-36 flex-col items-center justify-center rounded-xl bg-surface shadow-[var(--shadow-border)] sm:size-44">
            <span className="font-mono text-xs tracking-[0.28em] text-subtle">{t.prism.ir}</span>
            <span className="font-display mt-2 text-center text-lg tracking-[-0.03em]">
              {native ? t.prism.passthrough : t.prism.transcode}
            </span>
          </div>
          <SpokeList label={t.architecture.outbound} active={outbound} side="out" />
        </div>

        <p className="mt-10 max-w-2xl text-sm text-muted">
          {native ? t.architecture.nativeNote : t.architecture.transcodeNote}
        </p>
      </div>
    </section>
  )
}

function SpokeList({
  label,
  active,
  side,
}: {
  label: string
  active: ProtocolId
  side: 'in' | 'out'
}) {
  return (
    <div>
      <p className="mb-4 font-mono text-[0.68rem] tracking-[0.18em] text-subtle uppercase">
        {label}
      </p>
      <ul className="space-y-2">
        {PROTOCOLS.map((p) => {
          const on = p.id === active
          return (
            <li
              key={p.id}
              className={cn(
                'flex min-h-11 items-center justify-between rounded-lg px-3 shadow-[var(--shadow-border)] transition-colors duration-150',
                on ? 'bg-primary/10 text-fg' : 'text-muted',
                side === 'out' && 'flex-row-reverse',
              )}
            >
              <span className="font-mono text-xs">{p.name}</span>
              <span className={cn('size-1.5 rounded-full', on ? 'bg-primary' : 'bg-subtle/40')} />
            </li>
          )
        })}
      </ul>
    </div>
  )
}

function Endpoints({
  inbound,
  outbound,
  onInbound,
}: {
  inbound: ProtocolId
  outbound: ProtocolId
  onInbound: (id: ProtocolId) => void
}) {
  const { t } = useI18n()
  const from = protocolById(inbound)
  const to = protocolById(outbound)

  return (
    <section id="endpoints" className="mx-auto max-w-6xl px-5 py-24 sm:py-32">
      <SectionHead kicker={t.endpoints.kicker} title={t.endpoints.title} lede={t.endpoints.lede} />

      <div className="mt-10 overflow-hidden rounded-xl bg-bg-elevated shadow-[var(--shadow-border)]">
        <div className="flex gap-1 overflow-x-auto border-b border-border px-2 pt-2">
          {PROTOCOLS.map((p) => (
            <button
              key={p.id}
              type="button"
              onClick={() => onInbound(p.id)}
              className={cn(
                'relative min-h-11 shrink-0 px-4 font-mono text-xs transition-colors duration-150',
                inbound === p.id ? 'text-fg' : 'text-muted hover:text-fg',
              )}
            >
              {p.name}
              {inbound === p.id ? (
                <span className="absolute inset-x-3 bottom-0 h-px bg-primary" />
              ) : null}
            </button>
          ))}
        </div>
        <div className="flex flex-wrap items-center gap-2 border-b border-border px-5 py-3 font-mono text-[0.7rem] text-muted">
          <span>POST {from.path}</span>
          <span className="text-subtle">·</span>
          <span>
            {t.endpoints.via} {to.vendor} {to.name}
          </span>
        </div>
        <CodeBlock code={CURL[inbound]} />
      </div>
    </section>
  )
}

function CodeBlock({ code }: { code: string }) {
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)

  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(code)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1600)
    } catch {
      /* ignore */
    }
  }

  return (
    <div className="relative">
      <button
        type="button"
        onClick={() => void handleCopy()}
        className="absolute top-3 right-3 inline-flex h-9 items-center gap-1.5 rounded-full px-3 font-mono text-[0.68rem] text-muted shadow-[var(--shadow-border)] transition-colors hover:text-fg"
      >
        {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
        {copied ? t.endpoints.copied : t.endpoints.copy}
      </button>
      <pre className="overflow-x-auto p-5 pr-28 font-mono text-[0.78rem] leading-relaxed text-fg/85">
        <code>{code}</code>
      </pre>
    </div>
  )
}

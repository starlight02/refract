import { useId, useMemo } from 'react'
import { useI18n } from '@/lib/i18n'
import { PROTOCOLS, protocolById, type ProtocolId } from '@/lib/protocols'
import { cn } from '@/lib/utils'

const TOP: [number, number] = [400, 36]
const LEFT: [number, number] = [168, 368]
const RIGHT: [number, number] = [632, 368]

function lerp(a: number, b: number, t: number) {
  return a + (b - a) * t
}

function pointOnEdge(a: [number, number], b: [number, number], y: number): [number, number] {
  const t = (y - a[1]) / (b[1] - a[1])
  const tt = Math.min(0.88, Math.max(0.14, t))
  return [lerp(a[0], b[0], tt), lerp(a[1], b[1], tt)]
}

function yFor(id: ProtocolId) {
  const i = PROTOCOLS.findIndex((p) => p.id === id)
  return [72, 168, 264, 360][i] ?? 168
}

function rayPath(fromId: ProtocolId, toId: ProtocolId) {
  const fromY = yFor(fromId)
  const toY = yFor(toId)
  const [ex, ey] = pointOnEdge(TOP, LEFT, fromY)
  const [xx, xy] = pointOnEdge(TOP, RIGHT, toY)
  const same = fromId === toId
  const cx = 400
  const cy = same ? (ey + xy) / 2 : (ey + xy) / 2 + (toY - fromY) * 0.16
  return `M 8 ${fromY} L ${ex} ${ey} Q ${cx} ${cy} ${xx} ${xy} L 792 ${toY}`
}

function ProtocolDot({
  active,
  label,
  vendor,
  onSelect,
  align,
}: {
  active: boolean
  label: string
  vendor: string
  onSelect: () => void
  align: 'left' | 'right'
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-pressed={active}
      className={cn(
        'group flex min-h-11 items-center gap-3 rounded-lg px-1 py-1 text-left transition-colors duration-150',
        align === 'right' && 'flex-row-reverse text-right',
        active ? 'text-fg' : 'text-muted hover:text-fg',
      )}
    >
      <span
        className={cn(
          'size-2.5 shrink-0 rounded-full transition-[background-color,box-shadow] duration-150',
          active
            ? 'bg-primary shadow-[0_0_0_4px_color-mix(in_oklab,var(--color-primary)_22%,transparent)]'
            : 'bg-transparent shadow-[0_0_0_1px_var(--color-border-strong)] group-hover:shadow-[0_0_0_1px_var(--color-primary)]',
        )}
      />
      <span className="min-w-0">
        <span className="block font-mono text-[0.72rem] tracking-wide">{label}</span>
        <span className="block text-[0.68rem] text-subtle">{vendor}</span>
      </span>
    </button>
  )
}

export function PrismStage({
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
  const { t } = useI18n()
  const uid = useId()
  const native = inbound === outbound
  const d = useMemo(() => rayPath(inbound, outbound), [inbound, outbound])
  const from = protocolById(inbound)
  const to = protocolById(outbound)

  return (
    <div>
      <div className="mb-5 flex flex-wrap items-end justify-between gap-3">
        <p className="font-mono text-xs tracking-[0.14em] text-subtle uppercase">
          {from.vendor} {from.name}
          <span className="mx-2 text-subtle/50">→</span>
          {t.prism.ir}
          <span className="mx-2 text-subtle/50">→</span>
          {to.vendor} {to.name}
        </p>
        <p className="font-mono text-xs text-muted">
          {native ? t.prism.native : t.prism.transcoded}
        </p>
      </div>

      <div className="hidden md:grid md:grid-cols-[10.5rem_minmax(0,1fr)_10.5rem] md:items-stretch md:gap-2">
        <div className="flex flex-col justify-between py-2">
          {PROTOCOLS.map((p) => (
            <ProtocolDot
              key={p.id}
              active={inbound === p.id}
              label={p.name}
              vendor={p.vendor}
              onSelect={() => onInbound(p.id)}
              align="left"
            />
          ))}
        </div>

        <svg
          viewBox="0 0 800 400"
          className="h-auto w-full"
          role="img"
          aria-label={`${from.name} → ${to.name}`}
        >
          <defs>
            <linearGradient id={`${uid}-glass`} x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor="var(--color-primary)" stopOpacity="0.2" />
              <stop offset="100%" stopColor="var(--color-primary)" stopOpacity="0.05" />
            </linearGradient>
          </defs>

          <line x1="0" y1="200" x2="800" y2="200" stroke="var(--color-line)" strokeWidth="1" />

          <polygon
            points={`${TOP[0]},${TOP[1]} ${RIGHT[0]},${RIGHT[1]} ${LEFT[0]},${LEFT[1]}`}
            fill={`url(#${uid}-glass)`}
            stroke="var(--color-primary)"
            strokeOpacity="0.7"
            strokeWidth="1.4"
            strokeLinejoin="round"
          />
          <line
            x1={TOP[0]}
            y1={TOP[1]}
            x2={400}
            y2={368}
            stroke="var(--color-primary)"
            strokeOpacity="0.28"
            strokeWidth="1"
          />
          <text
            x="400"
            y="214"
            textAnchor="middle"
            fill="var(--color-fg)"
            fontSize="14"
            fontFamily="var(--font-mono)"
            letterSpacing="0.32em"
          >
            {t.prism.ir}
          </text>
          <text
            x="400"
            y="236"
            textAnchor="middle"
            fill="var(--color-subtle)"
            fontSize="10"
            fontFamily="var(--font-mono)"
          >
            {t.prism.irFull}
          </text>

          <path
            d={d}
            fill="none"
            stroke="var(--color-primary)"
            strokeOpacity="0.35"
            strokeWidth="2.4"
            strokeLinecap="round"
          />
          <path
            key={`${inbound}-${outbound}`}
            d={d}
            fill="none"
            stroke="var(--color-fg)"
            strokeWidth="1.7"
            strokeLinecap="round"
            className="ray-photon"
          />
        </svg>

        <div className="flex flex-col justify-between py-2">
          {PROTOCOLS.map((p) => (
            <ProtocolDot
              key={p.id}
              active={outbound === p.id}
              label={p.name}
              vendor={p.vendor}
              onSelect={() => onOutbound(p.id)}
              align="right"
            />
          ))}
        </div>
      </div>

      <div className="md:hidden">
        <MobilePrism
          inbound={inbound}
          outbound={outbound}
          native={native}
          onInbound={onInbound}
          onOutbound={onOutbound}
        />
      </div>
    </div>
  )
}

function MobilePrism({
  inbound,
  outbound,
  native,
  onInbound,
  onOutbound,
}: {
  inbound: ProtocolId
  outbound: ProtocolId
  native: boolean
  onInbound: (id: ProtocolId) => void
  onOutbound: (id: ProtocolId) => void
}) {
  const { t } = useI18n()
  return (
    <div className="rounded-xl bg-bg p-4 shadow-[var(--shadow-border)]">
      <p className="mb-2 font-mono text-[0.68rem] tracking-[0.16em] text-subtle uppercase">
        {t.prism.inbound}
      </p>
      <ChipRow value={inbound} onChange={onInbound} />
      <div className="my-5 flex flex-col items-center gap-2">
        <span className="h-8 w-px bg-border-strong" />
        <div className="flex size-16 items-center justify-center rounded-lg bg-surface shadow-[var(--shadow-border)]">
          <span className="font-mono text-xs tracking-[0.22em]">{t.prism.ir}</span>
        </div>
        <span className="h-8 w-px bg-border-strong" />
        <p className="text-center font-mono text-[0.68rem] text-muted">
          {native ? t.prism.native : t.prism.transcoded}
        </p>
      </div>
      <p className="mb-2 font-mono text-[0.68rem] tracking-[0.16em] text-subtle uppercase">
        {t.prism.outbound}
      </p>
      <ChipRow value={outbound} onChange={onOutbound} />
    </div>
  )
}

function ChipRow({ value, onChange }: { value: ProtocolId; onChange: (id: ProtocolId) => void }) {
  return (
    <div className="grid grid-cols-2 gap-2">
      {PROTOCOLS.map((p) => {
        const active = value === p.id
        return (
          <button
            key={p.id}
            type="button"
            onClick={() => onChange(p.id)}
            aria-pressed={active}
            className={cn(
              'flex min-h-11 flex-col items-start justify-center rounded-lg px-3 py-2 text-left transition-[background-color,box-shadow,color] duration-150',
              active
                ? 'bg-primary/10 text-fg shadow-[0_0_0_1px_color-mix(in_oklab,var(--color-primary)_45%,transparent)]'
                : 'text-muted shadow-[var(--shadow-border)] hover:text-fg',
            )}
          >
            <span className="font-mono text-xs">{p.name}</span>
            <span className="text-[0.65rem] text-subtle">{p.vendor}</span>
          </button>
        )
      })}
    </div>
  )
}

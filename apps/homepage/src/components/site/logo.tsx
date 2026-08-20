import { cn } from '@/lib/utils'

export function Mark({ className }: { className?: string }) {
  return (
    <img
      src="/favicon.svg"
      alt=""
      className={cn('size-7 shrink-0', className)}
      aria-hidden="true"
      draggable={false}
    />
  )
}

export function Wordmark({ className }: { className?: string }) {
  return (
    <span className={cn('inline-flex items-center gap-2.5', className)}>
      <Mark />
      <span className="font-display text-[1.05rem] font-medium tracking-[-0.03em]">Refract</span>
    </span>
  )
}

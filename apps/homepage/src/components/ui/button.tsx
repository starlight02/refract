import { Slot } from '@radix-ui/react-slot'
import { cva, type VariantProps } from 'class-variance-authority'
import type { ButtonHTMLAttributes } from 'react'
import { cn } from '@/lib/utils'

const buttonVariants = cva(
  'inline-flex items-center justify-center gap-2 font-medium transition-[color,background-color,box-shadow,transform,opacity] duration-150 ease-out focus-visible:outline-none disabled:pointer-events-none disabled:opacity-40 active:not-disabled:scale-[0.96] select-none',
  {
    variants: {
      variant: {
        primary:
          'bg-primary text-primary-fg shadow-[0_0_0_1px_color-mix(in_oklab,var(--color-primary)_70%,transparent)] hover:opacity-90',
        outline:
          'bg-transparent text-fg shadow-[var(--shadow-border)] hover:shadow-[var(--shadow-border-hover)] hover:bg-fg/4',
        ghost: 'bg-transparent text-muted hover:text-fg hover:bg-fg/5',
      },
      size: {
        sm: 'h-9 rounded-full px-3.5 text-sm',
        md: 'h-11 rounded-full px-5 text-sm',
        lg: 'h-12 rounded-full px-6 text-[0.9375rem]',
      },
    },
    defaultVariants: {
      variant: 'primary',
      size: 'md',
    },
  },
)

type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> &
  VariantProps<typeof buttonVariants> & {
    asChild?: boolean
  }

function Button({ className, variant, size, asChild = false, ...props }: ButtonProps) {
  const Comp = asChild ? Slot : 'button'
  return <Comp className={cn(buttonVariants({ variant, size }), className)} {...props} />
}

export { Button, buttonVariants }

import { cva, type VariantProps } from "class-variance-authority";
import { cn, focusRing } from "@/lib/cn";

/* design/05 §3.1 — macOS control heights (28/32/36px), NOT the web default. */
const button = cva(
  cn(
    "inline-flex items-center justify-center gap-1.5 rounded-md text-callout font-medium",
    "transition-colors duration-[120ms] disabled:pointer-events-none disabled:opacity-50",
    focusRing
  ),
  {
    variants: {
      variant: {
        primary:
          "bg-primary text-primary-foreground hover:bg-primary/90 active:bg-primary/80",
        secondary:
          "bg-card-raised text-foreground border border-border hover:bg-card-raised/80",
        danger:
          "bg-card-raised text-danger border border-danger/40 hover:bg-danger/10",
        ghost:
          "text-muted-foreground hover:text-foreground hover:bg-card-raised/60",
        link: "text-muted-foreground hover:text-foreground underline-offset-4 hover:underline",
      },
      size: {
        sm: "h-7 px-2.5",
        md: "h-8 px-3",
        lg: "h-9 px-4",
        icon: "size-8",
      },
    },
    defaultVariants: { variant: "primary", size: "md" },
  }
);

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof button> {
  loading?: boolean;
}

export function Button({
  className,
  variant,
  size,
  loading,
  disabled,
  children,
  ...props
}: ButtonProps) {
  return (
    <button
      className={cn(button({ variant, size }), className)}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      {...props}
    >
      {loading && <Spinner />}
      {children}
    </button>
  );
}

export function Spinner() {
  return (
    <svg
      className="size-3.5 animate-spin"
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
    >
      <circle cx="8" cy="8" r="6" stroke="currentColor" strokeOpacity="0.25" strokeWidth="2" />
      <path d="M14 8a6 6 0 0 0-6-6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
    </svg>
  );
}

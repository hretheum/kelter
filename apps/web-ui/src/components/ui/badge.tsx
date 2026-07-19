import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/cn";

const badge = cva(
  "inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-caption2 font-medium",
  {
    variants: {
      tone: {
        quiet: "bg-card-raised text-muted-foreground",
        accent: "bg-primary/15 text-primary",
        success: "bg-success/15 text-success",
        danger: "bg-danger/15 text-danger",
        warning: "bg-warning/15 text-warning",
      },
    },
    defaultVariants: { tone: "quiet" },
  }
);

export function Badge({
  className,
  tone,
  ...props
}: React.HTMLAttributes<HTMLSpanElement> & VariantProps<typeof badge>) {
  return <span className={cn(badge({ tone }), className)} {...props} />;
}

export function Skeleton({ className }: { className?: string }) {
  return (
    <div
      className={cn("animate-pulse rounded-md bg-card-raised", className)}
      aria-hidden="true"
    />
  );
}

export function Banner({
  className,
  tone = "warning",
  children,
}: {
  className?: string;
  tone?: "info" | "warning" | "danger";
  children: React.ReactNode;
}) {
  const tones = {
    info: "border-border bg-card text-muted-foreground",
    warning: "border-warning/40 bg-warning/10 text-warning",
    danger: "border-danger/40 bg-danger/10 text-danger",
  };
  return (
    <div
      role="status"
      className={cn(
        "rounded-md border px-3 py-2 text-caption",
        tones[tone],
        className
      )}
    >
      {children}
    </div>
  );
}

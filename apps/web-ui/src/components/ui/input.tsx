import { cn, focusRing } from "@/lib/cn";

export interface InputProps extends React.InputHTMLAttributes<HTMLInputElement> {
  error?: boolean;
}

export function Input({ className, error, ...props }: InputProps) {
  return (
    <input
      className={cn(
        "block w-full h-8 rounded-md border border-border bg-input px-2.5 text-body",
        "text-foreground placeholder:text-muted-foreground",
        "disabled:opacity-50 disabled:cursor-not-allowed",
        error && "border-danger",
        focusRing,
        className
      )}
      aria-invalid={error || undefined}
      {...props}
    />
  );
}

export function Textarea({
  className,
  error,
  ...props
}: React.TextareaHTMLAttributes<HTMLTextAreaElement> & {
  error?: boolean;
  ref?: React.Ref<HTMLTextAreaElement>; // React 19: ref is a regular prop
}) {
  return (
    <textarea
      className={cn(
        "block w-full rounded-md border border-border bg-input px-2.5 py-2 text-body",
        "text-foreground placeholder:text-muted-foreground resize-none",
        "disabled:opacity-50 disabled:cursor-not-allowed",
        error && "border-danger",
        focusRing,
        className
      )}
      aria-invalid={error || undefined}
      {...props}
    />
  );
}

export function Select({
  className,
  ...props
}: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select
      className={cn(
        "block w-full h-8 rounded-md border border-border bg-input px-2 text-body",
        "text-foreground disabled:opacity-50 disabled:cursor-not-allowed",
        focusRing,
        className
      )}
      {...props}
    />
  );
}

export function Label({
  className,
  ...props
}: React.LabelHTMLAttributes<HTMLLabelElement>) {
  return (
    <label
      className={cn("block text-caption text-muted-foreground font-medium", className)}
      {...props}
    />
  );
}

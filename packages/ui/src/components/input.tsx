import type { InputHTMLAttributes } from "react";

import { cn } from "../lib/utils";

export function Input({ className, ...props }: InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={cn(
        "h-9 w-full rounded-lg border border-border bg-surface px-2.5 text-xs text-secondary outline-none transition-colors placeholder:text-muted-foreground focus:border-primary/40 focus:ring-2 focus:ring-primary/15 disabled:opacity-50",
        className,
      )}
      {...props}
    />
  );
}

import { cva, type VariantProps } from "class-variance-authority";
import type { HTMLAttributes } from "react";

import { cn } from "../lib/utils";

export const badgeVariants = cva(
  "inline-flex items-center gap-1.5 rounded-md border px-2 py-0.5 text-[11px] font-medium leading-4",
  {
    variants: {
      variant: {
        accent: "border-primary-line bg-primary-soft text-primary",
        neutral: "border-border bg-raised text-secondary",
        outline: "border-border bg-transparent text-secondary",
        warning: "border-amber/30 bg-amber/10 text-amber",
        violet: "border-violet/30 bg-violet/10 text-violet",
        danger: "border-danger/30 bg-danger/10 text-danger",
      },
    },
    defaultVariants: { variant: "neutral" },
  },
);

export type BadgeProps = HTMLAttributes<HTMLSpanElement> & VariantProps<typeof badgeVariants>;

export function Badge({ className, variant, ...props }: BadgeProps) {
  return <span className={cn(badgeVariants({ variant }), className)} {...props} />;
}

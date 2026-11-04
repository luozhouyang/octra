import { cva, type VariantProps } from "class-variance-authority";
import type { ButtonHTMLAttributes } from "react";

import { cn } from "../lib/utils";

export const buttonVariants = cva(
  "inline-flex shrink-0 select-none items-center justify-center gap-2 whitespace-nowrap rounded-lg font-medium transition-colors duration-150 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary/40 disabled:pointer-events-none disabled:opacity-45",
  {
    variants: {
      variant: {
        default: "bg-primary text-primary-foreground hover:bg-primary-hover",
        secondary:
          "border border-border bg-card text-foreground hover:border-border-strong hover:bg-raised",
        outline: "border border-border bg-transparent text-foreground hover:bg-raised",
        ghost: "text-secondary hover:bg-raised hover:text-foreground",
        danger: "border border-danger/30 bg-danger/10 text-danger hover:bg-danger/15",
      },
      size: {
        default: "h-9 px-3.5 text-xs",
        sm: "h-8 px-3 text-[11px]",
        lg: "h-10 px-5 text-sm",
        icon: "h-8 w-8",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  },
);

export type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> &
  VariantProps<typeof buttonVariants>;

export function Button({ className, variant, size, type = "button", ...props }: ButtonProps) {
  return (
    <button type={type} className={cn(buttonVariants({ variant, size }), className)} {...props} />
  );
}

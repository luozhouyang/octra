import type { ReactNode } from "react";

interface PageHeaderProps {
  eyebrow: string;
  title: string;
  description: string;
  actions?: ReactNode;
}

export function PageHeader({ eyebrow, title, description, actions }: PageHeaderProps) {
  return (
    <header className="flex flex-col gap-4 sm:flex-row sm:items-end sm:justify-between">
      <div className="flex min-w-0 flex-col gap-2">
        <div className="flex items-center gap-2">
          <span className="h-1.5 w-1.5 rounded-full bg-primary" />
          <span className="font-mono text-[11px] text-muted-foreground">{eyebrow}</span>
        </div>
        <h1 className="text-[28px] font-medium leading-[1.1] tracking-[-0.01em] sm:text-[32px]">
          {title}
        </h1>
        <p className="max-w-[640px] text-[13px] leading-5 text-secondary">{description}</p>
      </div>
      {actions ? (
        <div className="flex flex-wrap items-center gap-2.5 sm:shrink-0">{actions}</div>
      ) : null}
    </header>
  );
}

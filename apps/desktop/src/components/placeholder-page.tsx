interface PlaceholderPageProps {
  title: string;
  phase: string;
  description: string;
  capabilities: string[];
}

export function PlaceholderPage({ title, phase, description, capabilities }: PlaceholderPageProps) {
  return (
    <div className="space-y-8">
      <header>
        <p className="text-xs font-medium uppercase tracking-[0.2em] text-muted-foreground">
          {phase}
        </p>
        <h1 className="mt-2 text-2xl font-semibold">{title}</h1>
        <p className="mt-2 max-w-2xl text-sm text-muted-foreground">{description}</p>
      </header>

      <section className="rounded-lg border border-border bg-card p-6">
        <h2 className="text-sm font-medium">Planned capabilities</h2>
        <ul className="mt-4 space-y-2 text-sm text-muted-foreground">
          {capabilities.map((capability) => (
            <li key={capability} className="flex gap-3">
              <span className="mt-2 h-1.5 w-1.5 shrink-0 rounded-full bg-primary" />
              {capability}
            </li>
          ))}
        </ul>
      </section>

      <p className="text-xs text-muted-foreground">
        This screen is a scaffold. The daemon and control API behind it are live.
      </p>
    </div>
  );
}

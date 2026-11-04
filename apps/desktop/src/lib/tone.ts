export type Tone = "primary" | "sky" | "violet" | "amber" | "coral" | "muted" | "danger";

export const toneText: Record<Tone, string> = {
  primary: "text-primary",
  sky: "text-sky",
  violet: "text-violet",
  amber: "text-amber",
  coral: "text-coral",
  muted: "text-muted-foreground",
  danger: "text-danger",
};

export const toneDot: Record<Tone, string> = {
  primary: "bg-primary",
  sky: "bg-sky",
  violet: "bg-violet",
  amber: "bg-amber",
  coral: "bg-coral",
  muted: "bg-muted-foreground",
  danger: "bg-danger",
};

export const toneSoft: Record<Tone, string> = {
  primary: "bg-primary-soft text-primary",
  sky: "bg-sky/10 text-sky",
  violet: "bg-violet/10 text-violet",
  amber: "bg-amber/10 text-amber",
  coral: "bg-coral/10 text-coral",
  muted: "bg-raised text-secondary",
  danger: "bg-danger/10 text-danger",
};

export const toneBar: Record<Tone, string> = {
  primary: "bg-primary",
  sky: "bg-sky",
  violet: "bg-violet",
  amber: "bg-amber",
  coral: "bg-coral",
  muted: "bg-muted-foreground",
  danger: "bg-danger",
};

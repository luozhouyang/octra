import { cn } from "../lib/utils";

export interface SegmentedOption<T extends string> {
  value: T;
  label: string;
}

interface SegmentedProps<T extends string> {
  options: readonly SegmentedOption<T>[];
  value: T;
  onChange?: (value: T) => void;
  size?: "sm" | "md";
  className?: string;
}

export function Segmented<T extends string>({
  options,
  value,
  onChange,
  size = "md",
  className,
}: SegmentedProps<T>) {
  return (
    <div
      role="group"
      className={cn(
        "inline-flex items-center gap-0.5 rounded-lg border border-border bg-surface p-0.5",
        className,
      )}
    >
      {options.map((option) => {
        const active = option.value === value;
        return (
          <button
            key={option.value}
            type="button"
            aria-pressed={active}
            onClick={() => onChange?.(option.value)}
            className={cn(
              "inline-flex items-center justify-center rounded-md font-medium transition-colors duration-150",
              size === "sm" ? "h-6 px-2 text-[10.5px]" : "h-[26px] px-2.5 text-[11px]",
              active ? "bg-raised text-foreground" : "text-muted-foreground hover:text-secondary",
            )}
          >
            {option.label}
          </button>
        );
      })}
    </div>
  );
}

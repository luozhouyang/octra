import { cn } from "../lib/utils";

interface SwitchProps {
  checked: boolean;
  onCheckedChange?: (checked: boolean) => void;
  disabled?: boolean;
  label?: string;
  className?: string;
}

export function Switch({ checked, onCheckedChange, disabled, label, className }: SwitchProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onCheckedChange?.(!checked)}
      className={cn(
        "inline-flex h-[18px] w-8 shrink-0 items-center rounded-full p-[3px] transition-colors duration-150 disabled:opacity-45",
        checked ? "justify-end bg-primary" : "justify-start bg-raised",
        className,
      )}
    >
      <span
        className={cn(
          "h-3 w-3 rounded-full transition-transform duration-150",
          checked ? "bg-primary-foreground" : "bg-muted-foreground",
        )}
      />
    </button>
  );
}

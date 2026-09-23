import { Monitor, Moon, Sun } from "lucide-react";
import { useTheme, type ThemeChoice } from "@/lib/theme";
import { cn } from "@/lib/utils";

const OPTIONS: { id: ThemeChoice; label: string; icon: typeof Sun }[] = [
  { id: "light", label: "Light", icon: Sun },
  { id: "dark", label: "Dark", icon: Moon },
  { id: "system", label: "Automatic", icon: Monitor },
];

export function ThemeControl({
  compact = false,
  className,
}: {
  compact?: boolean;
  className?: string;
}) {
  const { choice, setTheme } = useTheme();

  return (
    <div
      role="radiogroup"
      aria-label="Theme"
      className={cn("flex gap-1 rounded-xl bg-surface-2 p-1", className)}
    >
      {OPTIONS.map(({ id, label, icon: Icon }) => {
        const active = choice === id;
        return (
          <button
            key={id}
            role="radio"
            aria-checked={active}
            aria-label={label}
            title={compact ? label : undefined}
            onClick={() => setTheme(id)}
            className={cn(
              "flex flex-1 items-center justify-center gap-1.5 rounded-lg px-3 py-2 text-sm transition-colors",
              active
                ? "bg-surface font-medium text-fg shadow-[var(--shadow-card)]"
                : "text-fg-muted hover:text-fg",
            )}
          >
            <Icon className="h-4 w-4 shrink-0" strokeWidth={1.75} />
            {!compact && <span>{label}</span>}
          </button>
        );
      })}
    </div>
  );
}

import { cn } from "@/lib/utils";

export function Logo({ className }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 32 32"
      fill="none"
      className={cn("text-primary", className)}
      aria-hidden
    >
      {}
      <path
        d="M3.5 14.2 16 4.2l12.5 10"
        stroke="currentColor"
        strokeWidth="2.6"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      {}
      <rect
        x="6.4"
        y="14.6"
        width="19.2"
        height="13.2"
        rx="2.6"
        stroke="currentColor"
        strokeWidth="2.4"
      />
      {}
      <path
        d="M6.4 21.2h19.2"
        stroke="currentColor"
        strokeWidth="1.7"
        strokeLinecap="round"
      />
      <circle cx="10.4" cy="17.9" r="1.35" fill="currentColor" />
      <circle cx="10.4" cy="24.5" r="1.35" fill="currentColor" />
    </svg>
  );
}

export function Wordmark({
  className,
  size = "md",
}: {
  className?: string;
  size?: "md" | "lg";
}) {
  return (
    <span className={cn("flex items-center gap-2.5", className)}>
      <Logo className={size === "lg" ? "h-8 w-8" : "h-6 w-6"} />
      <span
        className={cn(
          "font-display text-fg",
          size === "lg" ? "text-2xl" : "text-lg",
        )}
      >
        YoLab
      </span>
    </span>
  );
}

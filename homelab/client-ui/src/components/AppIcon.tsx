import { useState } from "react";
import { CATALOG_ICONS } from "@/catalog/icons";
import { cn } from "@/lib/utils";

export function AppIcon({
  appId,
  icon,
  name,
  className,
}: {
  appId?: string;
  icon?: string;
  name: string;
  className?: string;
}) {
  const [failed, setFailed] = useState(false);

  const bundled = appId ? CATALOG_ICONS[appId] : undefined;
  const remote =
    icon && (/^(https?:)?\/\//.test(icon) || icon.startsWith("/"))
      ? icon
      : undefined;
  const src = bundled ?? remote;

  if (src && !failed) {
    return (
      <img
        src={src}
        alt=""
        loading="lazy"
        referrerPolicy="no-referrer"
        onError={() => setFailed(true)}
        className={cn("object-contain", className)}
      />
    );
  }

  if (icon && !remote) {
    return (
      <span className={cn("leading-none", className)} aria-hidden>
        {icon}
      </span>
    );
  }

  return (
    <span
      className={cn(
        "flex items-center justify-center font-semibold text-fg-muted",
        className,
      )}
      aria-hidden
    >
      {(name || "?").charAt(0).toUpperCase()}
    </span>
  );
}

export function AppIconTile({
  appId,
  icon,
  name,
  size = "md",
  className,
}: {
  appId?: string;
  icon?: string;
  name: string;
  size?: "sm" | "md" | "lg";
  className?: string;
}) {
  const box = {
    sm: "h-12 w-12 rounded-xl",
    md: "h-16 w-16 rounded-tile",
    lg: "h-16 w-16 rounded-tile",
  }[size];
  const glyph = {
    sm: "h-7 w-7 text-2xl",
    md: "h-9 w-9 text-3xl",
    lg: "h-9 w-9 text-3xl",
  }[size];

  return (
    <div
      className={cn(
        "flex shrink-0 items-center justify-center border border-border bg-[var(--icon-plate)] p-2",
        box,
        className,
      )}
    >
      <AppIcon appId={appId} icon={icon} name={name} className={glyph} />
    </div>
  );
}

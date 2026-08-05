import { useState } from "react";
import { CATALOG_ICONS } from "@/catalog/icons";
import { cn } from "@/lib/utils";

/**
 * An app's icon: the project's own logo where we have one, otherwise the emoji
 * from its Chart.yaml, otherwise its first letter.
 *
 * The emoji were placeholders that read as a toy. 52 of the 54 catalog apps
 * have a real mark, and a grid of those is instantly scannable in a way that
 * 📸 🎬 📚 is not — people recognise the Immich logo, they do not recognise a
 * camera emoji as meaning Immich specifically.
 *
 * `yolab.io/icon` stays as the fallback rather than being replaced, so an app
 * from a user-added repo, or one whose logo nobody has published, still gets
 * something. strfry and valheim are the two in the official catalog.
 */
export function AppIcon({
  appId,
  icon,
  name,
  className,
}: {
  /** Chart id, used to look up a bundled logo. */
  appId?: string;
  /** `yolab.io/icon`: usually an emoji, occasionally a remote URL. */
  icon?: string;
  name: string;
  /** Sizing for the glyph itself; the caller owns the surrounding tile. */
  className?: string;
}) {
  const [failed, setFailed] = useState(false);

  const bundled = appId ? CATALOG_ICONS[appId] : undefined;
  // Two charts point `yolab.io/icon` at a remote PNG/SVG. Those still work, but
  // they are a fallback now — a remote image fails in the case this product is
  // most likely to be in, a house whose internet is down.
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

/**
 * The rounded plate a logo sits on.
 *
 * Always light, in both themes. Almost every one of these logos was drawn for
 * a white background, and a fair number are near-black — on a dark surface
 * they simply disappear. A phone home screen has the same problem and solves
 * it the same way: the icon carries its own plate rather than inheriting the
 * page. The dark-theme plate is a warm off-white rather than pure white so a
 * grid of them does not glare.
 */
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

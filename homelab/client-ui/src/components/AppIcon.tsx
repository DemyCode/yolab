import { useState } from "react";
import { cn } from "@/lib/utils";

/**
 * An app's icon, which is usually an emoji and occasionally a URL.
 *
 * `yolab.io/icon` is free-form: most charts carry an emoji, but appflowy and
 * synapse point at a remote PNG/SVG. Rendering the annotation as text — which
 * is what the tiles did at first — turns those two into a wall of
 * `https://raw.githubusercontent.com/...` where the logo should be.
 *
 * Remote images also fail in the case this product is most likely to be in:
 * a box at home with no working internet, which is exactly when someone opens
 * the dashboard to find out why. So a failed load falls back to the app's
 * first letter rather than a broken-image glyph, and `no-referrer` keeps us
 * from telling GitHub which apps this household runs.
 */
export function AppIcon({
  icon,
  name,
  className,
}: {
  icon: string;
  name: string;
  /** Sizing for the glyph itself; the caller owns the surrounding tile. */
  className?: string;
}) {
  const [failed, setFailed] = useState(false);
  const isRemote = /^(https?:)?\/\//.test(icon) || icon.startsWith("/");

  if (isRemote && !failed) {
    return (
      <img
        src={icon}
        alt=""
        loading="lazy"
        referrerPolicy="no-referrer"
        onError={() => setFailed(true)}
        className={cn("object-contain", className)}
      />
    );
  }

  if (isRemote || !icon) {
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

  return (
    <span className={cn("leading-none", className)} aria-hidden>
      {icon}
    </span>
  );
}

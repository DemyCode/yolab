import { Link } from "react-router-dom";
import { Check, ExternalLink } from "lucide-react";
import { AppIconTile } from "@/components/AppIcon";
import { taglineFor } from "@/catalog/meta";
import type { CatalogApp } from "@/types/apps";

export function AppCard({ app, count }: { app: CatalogApp; count: number }) {
  return (
    <Link
      to={`/add/${app.id}`}
      className="group relative flex items-start gap-3.5 rounded-card border border-border bg-surface p-4 transition hover:border-border-strong hover:shadow-[var(--shadow-card)] active:scale-[0.99]"
    >
      <AppIconTile appId={app.id} icon={app.icon} name={app.name} size="sm" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate font-medium text-fg">{app.name}</span>
          {count > 0 && (
            <span className="flex shrink-0 items-center gap-1 text-xs text-success">
              <Check className="h-3.5 w-3.5" />
              {count === 1 ? "Installed" : `${count} installed`}
            </span>
          )}
        </div>
        <p className="mt-0.5 line-clamp-2 text-sm text-fg-muted">
          {taglineFor(app)}
        </p>
        <div className="mt-1 flex items-center gap-3 text-xs">
          {count > 0 && (
            <span className="text-fg-subtle opacity-0 transition-opacity group-hover:opacity-100">
              Add another copy
            </span>
          )}
          {app.home && (
            <a
              href={app.home}
              target="_blank"
              rel="noreferrer noopener"
              onClick={(e) => e.stopPropagation()}
              className="inline-flex items-center gap-1 text-fg-subtle transition-colors hover:text-primary hover:underline"
            >
              <ExternalLink className="h-3 w-3" />
              What is it?
            </a>
          )}
        </div>
      </div>
    </Link>
  );
}

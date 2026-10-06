import { Link } from "react-router-dom";
import { Check, Star } from "lucide-react";
import { AppIconTile } from "@/components/AppIcon";
import { taglineFor } from "@/catalog/meta";
import { formatCount, shownRating, type AppStats } from "@/lib/store";
import { cn } from "@/lib/utils";
import type { CatalogApp } from "@/types/apps";

interface CardProps {
  app: CatalogApp;
  count: number;
  stats?: AppStats;
}

function Installed({ count }: { count: number }) {
  if (count === 0) return null;
  return (
    <span className="flex shrink-0 items-center gap-1 text-xs text-success">
      <Check className="h-3.5 w-3.5" aria-hidden />
      {count === 1 ? "Installed" : `${count} installed`}
    </span>
  );
}

function Facts({ app, stats }: { app: CatalogApp; stats?: AppStats }) {
  const rating = shownRating(stats);
  const starred = app.stars !== null && app.stars > 0;
  if (!starred && !rating) return null;
  return (
    <div className="flex items-center gap-3 font-mono text-xs tabular-nums text-fg-subtle">
      {starred && (
        <span
          className="inline-flex items-center gap-1"
          title={`${app.stars!.toLocaleString()} stars on GitHub`}
        >
          <Star className="h-3 w-3" aria-hidden />
          {formatCount(app.stars!)}
        </span>
      )}
      {rating && (
        <span title={`Rated ${rating.average} by ${rating.count} YoLab users`}>
          {rating.average}/5
        </span>
      )}
    </div>
  );
}

const cardSurface =
  "group rounded-card border border-border bg-surface transition hover:border-border-strong hover:shadow-[var(--shadow-lift)] active:scale-[0.99]";

export function AppCard({ app, count, stats }: CardProps) {
  return (
    <Link
      to={`/add/${app.id}`}
      className={cn(cardSurface, "flex h-full items-start gap-3.5 p-4")}
    >
      <AppIconTile appId={app.id} icon={app.icon} name={app.name} size="sm" />
      <div className="flex min-w-0 flex-1 flex-col gap-1">
        <div className="flex items-center gap-2">
          <span className="truncate font-medium text-fg">{app.name}</span>
          <Installed count={count} />
        </div>
        <p className="line-clamp-2 text-sm text-fg-muted">{taglineFor(app)}</p>
        <Facts app={app} stats={stats} />
      </div>
    </Link>
  );
}

export function FeaturedAppCard({ app, count, stats }: CardProps) {
  return (
    <Link
      to={`/add/${app.id}`}
      className={cn(
        cardSurface,
        "flex h-full items-start gap-4 p-5 sm:flex-col sm:gap-5",
      )}
    >
      <AppIconTile appId={app.id} icon={app.icon} name={app.name} size="lg" />
      <div className="flex min-w-0 flex-1 flex-col gap-1.5">
        <div className="flex items-center gap-2">
          <span className="truncate text-[1.05rem] font-semibold text-fg">
            {app.name}
          </span>
          <Installed count={count} />
        </div>
        <p className="text-sm text-fg-muted sm:line-clamp-3">
          {taglineFor(app)}
        </p>
        <div className="mt-auto pt-1.5">
          <Facts app={app} stats={stats} />
        </div>
      </div>
    </Link>
  );
}

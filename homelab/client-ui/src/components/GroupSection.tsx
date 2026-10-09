import { useState } from "react";
import { Link } from "react-router-dom";
import { ChevronDown, Settings2 } from "lucide-react";
import { AppTile } from "@/components/AppTile";
import { Collapse } from "@/components/motion";
import { appDisplayName, catalogEntry } from "@/lib/apps";
import type { HomeGroup } from "@/lib/groups";
import { cn } from "@/lib/utils";
import type { AppInfo, CatalogApp } from "@/types/apps";

export function GroupSection({
  group,
  catalog,
  installed,
}: {
  group: HomeGroup;
  catalog: CatalogApp[];
  installed: AppInfo[];
}) {
  const [open, setOpen] = useState(false);

  const tile = (app: AppInfo) => (
    <AppTile
      key={app.instance_name}
      app={app}
      name={appDisplayName(app, catalog, installed)}
      icon={catalogEntry(app, catalog)?.icon ?? "📦"}
    />
  );

  return (
    <section className="mt-8 rounded-card border border-border bg-surface p-3">
      <div className="flex items-center justify-between gap-3 px-2 pt-1">
        <h2 className="text-sm font-semibold text-fg">{group.title}</h2>
        <Link
          to={`/group/${group.name}`}
          className="inline-flex items-center gap-1.5 rounded-control px-2.5 py-1.5 text-sm font-medium text-primary hover:bg-primary-soft"
        >
          <Settings2 className="h-4 w-4" />
          Group
        </Link>
      </div>
      <div className="grid grid-cols-3 gap-2 sm:grid-cols-4 md:grid-cols-6">
        {group.main.map(tile)}
      </div>
      {group.others.length > 0 && (
        <>
          <button
            type="button"
            aria-expanded={open}
            onClick={() => setOpen((o) => !o)}
            className="mt-1 flex w-full items-center gap-2 rounded-control px-2 py-2 text-left text-sm text-fg-muted hover:bg-surface-2"
          >
            <ChevronDown
              className={cn(
                "h-4 w-4 transition-transform",
                open && "rotate-180",
              )}
            />
            Behind the scenes · {group.others.length}
          </button>
          <Collapse open={open}>
            <div className="grid grid-cols-3 gap-2 sm:grid-cols-4 md:grid-cols-6">
              {group.others.map(tile)}
            </div>
          </Collapse>
        </>
      )}
    </section>
  );
}

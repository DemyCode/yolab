import { useRef, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { ClipboardPaste, FileUp } from "lucide-react";
import { AppIconTile } from "@/components/AppIcon";
import { Button } from "@/components/ui/button";
import { Sheet } from "@/components/ui/sheet";
import { api } from "@/lib/api";
import type { CatalogSetup, Setup } from "@/lib/groups";
import { useApi } from "@/lib/useResource";
import type { CatalogApp } from "@/types/apps";

export function SetupsRow({ catalog }: { catalog: CatalogApp[] }) {
  const setups = useApi<CatalogSetup[]>("setups", "/api/setups");
  const navigate = useNavigate();
  const picker = useRef<HTMLInputElement>(null);
  const [error, setError] = useState<string | null>(null);
  const [pasting, setPasting] = useState(false);
  const [pasted, setPasted] = useState("");

  async function open(text: string) {
    setError(null);
    try {
      const setup = await api.post<Setup>("/api/setups/parse", { text });
      setPasting(false);
      navigate("/add/setup/file", { state: { setup } });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  const list = setups.data ?? [];

  return (
    <section className="mt-10">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h2 className="text-base font-semibold text-fg">Ready-made setups</h2>
          <p className="mt-0.5 text-sm text-fg-muted">
            Several apps that work together, installed in one go. Each one stays
            a normal app you can change or remove later.
          </p>
        </div>
        <div className="flex gap-1">
          <button
            type="button"
            onClick={() => {
              setError(null);
              setPasting(true);
            }}
            className="inline-flex items-center gap-1.5 rounded-control px-2.5 py-1.5 text-sm font-medium text-primary hover:bg-primary-soft"
          >
            <ClipboardPaste className="h-4 w-4" />
            Paste a setup
          </button>
          <button
            type="button"
            onClick={() => picker.current?.click()}
            className="inline-flex items-center gap-1.5 rounded-control px-2.5 py-1.5 text-sm font-medium text-primary hover:bg-primary-soft"
          >
            <FileUp className="h-4 w-4" />
            From a file
          </button>
        </div>
        <input
          ref={picker}
          type="file"
          accept=".yaml,.yml,application/yaml,text/yaml"
          className="hidden"
          onChange={(e) => {
            const file = e.target.files?.[0];
            e.target.value = "";
            if (file) void file.text().then(open);
          }}
        />
      </div>
      {error && !pasting && <p className="mt-2 text-sm text-danger">{error}</p>}
      <Sheet
        open={pasting}
        onClose={() => setPasting(false)}
        title="Paste a setup"
        subtitle="The text of a setup, as shown by “View as text” on a group."
        wide
      >
        <textarea
          value={pasted}
          rows={14}
          aria-label="Setup text"
          onChange={(e) => setPasted(e.target.value)}
          className="w-full rounded-control border border-border bg-surface px-3 py-2 font-mono text-sm text-fg focus:border-primary focus:outline-none"
        />
        {error && <p className="mt-2 text-sm text-danger">{error}</p>}
        <div className="mt-4 flex justify-end">
          <Button onClick={() => void open(pasted)} disabled={!pasted.trim()}>
            Continue
          </Button>
        </div>
      </Sheet>
      {list.length > 0 && (
        <div className="mt-3 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {list.map((s) => (
            <Link
              key={s.id}
              to={`/add/setup/${s.id}`}
              className="rounded-card border border-border bg-surface p-4 transition-colors hover:bg-surface-2"
            >
              <div className="flex -space-x-2">
                {Object.values(s.apps).map((a, i) => {
                  const app = catalog.find((c) => c.id === a.chart);
                  return (
                    <AppIconTile
                      key={`${a.chart}-${i}`}
                      appId={a.chart}
                      icon={app?.icon ?? "📦"}
                      name={app?.name ?? a.chart}
                      size="sm"
                      className="ring-2 ring-surface"
                    />
                  );
                })}
              </div>
              <div className="mt-3 text-sm font-semibold text-fg">
                {s.title}
              </div>
              {s.tagline && (
                <div className="mt-0.5 text-sm text-fg-muted">{s.tagline}</div>
              )}
            </Link>
          ))}
        </div>
      )}
    </section>
  );
}

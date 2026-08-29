import { useState } from "react";
import { useNavigate } from "react-router-dom";
import {
  AlertTriangle,
  Check,
  FileCode2,
  Package,
  Trash2,
  Upload,
} from "lucide-react";
import { Page } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { api } from "@/lib/api";
import { useResource } from "@/lib/useResource";

interface CustomApp {
  id: string;
  display_name: string;
  icon: string;
  description: string;
  port?: number;
  service?: string;
}

const EXAMPLE = `apiVersion: apps/v1
kind: Deployment
metadata:
  name: hello
spec:
  selector:
    matchLabels: { app: hello }
  template:
    metadata:
      labels: { app: hello }
    spec:
      containers:
        - name: hello
          image: nginx:1.27
          ports:
            - containerPort: 80
---
apiVersion: v1
kind: Service
metadata:
  name: hello
spec:
  selector: { app: hello }
  ports:
    - port: 80
      targetPort: 80
`;

/**
 * Bring your own app.
 *
 * The YAML is not applied here. It is turned into a chart that depends on the same
 * library every catalog app uses, so the result is an ordinary app: it appears on the
 * Apps page, gets a subdomain and a certificate, is backed up, and uninstalls
 * cleanly. Saving only adds it to the catalog — installing is the same form as
 * everything else, which is why there is no "install" button on this page.
 */
export default function CustomAppPage() {
  const navigate = useNavigate();
  const apps = useResource<CustomApp[]>("custom-apps", () =>
    api.get<CustomApp[]>("/api/apps/custom"),
  );

  const [id, setId] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [icon, setIcon] = useState("🔧");
  const [description, setDescription] = useState("");
  const [port, setPort] = useState("");
  const [service, setService] = useState("");
  const [yaml, setYaml] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const [uploadedId, setUploadedId] = useState<string | null>(null);

  /**
   * A packaged chart is sent as the request body rather than as multipart: the only
   * field is the file, and the body already is the file.
   */
  async function uploadChart(file: File) {
    setBusy(true);
    setError(null);
    setSaved(null);
    setUploadedId(null);
    try {
      const res = await fetch("/api/apps/custom/chart", {
        method: "POST",
        headers: { "Content-Type": "application/octet-stream" },
        body: file,
      });
      const d = (await res.json()) as {
        error?: string;
        app?: CustomApp;
        has_form?: boolean;
      };
      if (!res.ok || !d.app)
        throw new Error(d.error ?? `Server error ${res.status}`);
      setUploadedId(d.app.id);
      setSaved(
        d.has_form
          ? `${d.app.display_name} is in your catalog, with its own settings.`
          : `${d.app.display_name} is in your catalog.`,
      );
      await apps.refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not read that chart");
    } finally {
      setBusy(false);
    }
  }

  async function save() {
    setBusy(true);
    setError(null);
    setSaved(null);
    try {
      const res = await api.post<{ documents: number }>("/api/apps/custom", {
        id: id.trim(),
        display_name: displayName.trim(),
        icon: icon.trim(),
        description: description.trim(),
        yaml,
        port: port.trim() ? Number(port) : undefined,
        service: service.trim(),
      });
      setSaved(
        `Added. ${res.documents} object${res.documents === 1 ? "" : "s"} — it is in the catalog now.`,
      );
      await apps.refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not add that");
    } finally {
      setBusy(false);
    }
  }

  async function remove(app: CustomApp) {
    setBusy(true);
    setError(null);
    try {
      await api.del(`/api/apps/custom/${encodeURIComponent(app.id)}`);
      await apps.refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove that");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Page
      title="Add your own app"
      subtitle="Upload a Helm chart, or paste Kubernetes YAML. Either way it becomes an app like any other — its own address, its own backups."
    >
      <div className="space-y-4">
        {/* The packaged route comes first because it is the better one when it is
            available: a real chart already declares its own settings, so the
            install page shows them without anyone describing them twice here. */}
        <section className="rounded-card border border-border bg-surface p-4">
          <div className="flex flex-wrap items-center gap-3">
            <Package className="h-4 w-4 shrink-0 text-fg-muted" />
            <div className="min-w-0 flex-1">
              <h2 className="text-sm font-semibold text-fg">
                Already have a Helm chart?
              </h2>
              <p className="mt-0.5 text-sm text-fg-muted">
                Upload it as a <code>.tgz</code> or <code>.zip</code> and it
                installs with its own settings page, like every other app.
              </p>
            </div>
            <label className="shrink-0">
              <input
                type="file"
                accept=".tgz,.gz,.zip,application/zip,application/gzip"
                className="sr-only"
                disabled={busy}
                onChange={(e) => {
                  const f = e.target.files?.[0];
                  // Cleared so picking the same file twice fires again.
                  e.target.value = "";
                  if (f) void uploadChart(f);
                }}
              />
              <span className="inline-flex cursor-pointer items-center gap-1.5 rounded-md bg-primary px-3 py-1.5 text-sm font-medium text-primary-fg transition hover:opacity-90">
                <Upload className="h-3.5 w-3.5" />
                {busy ? "Checking…" : "Choose file"}
              </span>
            </label>
          </div>
          {uploadedId && (
            <Button
              size="sm"
              className="mt-3"
              onClick={() => navigate(`/add/${uploadedId}`)}
            >
              Set it up
            </Button>
          )}
        </section>

        <div className="flex items-center gap-3 py-1">
          <span className="h-px flex-1 bg-border" />
          <span className="text-xs text-fg-subtle">
            or paste plain Kubernetes YAML
          </span>
          <span className="h-px flex-1 bg-border" />
        </div>

        <div className="grid gap-3 sm:grid-cols-2">
          <label className="block">
            <span className="mb-1 block text-sm font-medium text-fg">Name</span>
            <Input
              value={displayName}
              onChange={(e) => {
                setDisplayName(e.target.value);
                if (!id)
                  setId(
                    e.target.value
                      .toLowerCase()
                      .replace(/[^a-z0-9]+/g, "-")
                      .replace(/^-+|-+$/g, ""),
                  );
              }}
              placeholder="My Thing"
            />
          </label>
          <label className="block">
            <span className="mb-1 block text-sm font-medium text-fg">
              Short id
            </span>
            <Input
              value={id}
              onChange={(e) => setId(e.target.value)}
              placeholder="my-thing"
              className="font-mono"
            />
          </label>
          <label className="block">
            <span className="mb-1 block text-sm font-medium text-fg">Icon</span>
            <Input
              value={icon}
              onChange={(e) => setIcon(e.target.value)}
              placeholder="🔧"
            />
          </label>
          <label className="block">
            <span className="mb-1 block text-sm font-medium text-fg">
              Web port
            </span>
            <Input
              value={port}
              onChange={(e) => setPort(e.target.value)}
              placeholder="80"
              inputMode="numeric"
            />
            <span className="mt-1 block text-xs text-fg-muted">
              The port your app listens on. Leave empty if it has no web page —
              it will run without an address of its own.
            </span>
          </label>
          <label className="block sm:col-span-2">
            <span className="mb-1 block text-sm font-medium text-fg">
              Service name
            </span>
            <Input
              value={service}
              onChange={(e) => setService(e.target.value)}
              placeholder="hello"
              className="font-mono"
            />
            <span className="mt-1 block text-xs text-fg-muted">
              The name of the Service in your YAML that should receive traffic.
            </span>
          </label>
          <label className="block sm:col-span-2">
            <span className="mb-1 block text-sm font-medium text-fg">
              What it is
            </span>
            <Input
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="One line, shown in the catalog"
            />
          </label>
        </div>

        <label className="block">
          <div className="mb-1 flex items-center justify-between">
            <span className="text-sm font-medium text-fg">Kubernetes YAML</span>
            <button
              type="button"
              onClick={() => setYaml(EXAMPLE)}
              className="text-xs text-fg-muted hover:text-fg"
            >
              Use an example
            </button>
          </div>
          <textarea
            value={yaml}
            onChange={(e) => setYaml(e.target.value)}
            spellCheck={false}
            rows={16}
            placeholder="apiVersion: apps/v1&#10;kind: Deployment&#10;…"
            aria-label="Kubernetes YAML"
            className="w-full rounded-md border border-border bg-bg p-3 font-mono text-xs text-fg outline-none focus:border-primary focus:ring-1 focus:ring-primary"
          />
          <span className="mt-1 block text-xs text-fg-muted">
            Leave out <code>namespace:</code> — YoLab gives this app its own.
            Anything that reaches outside that namespace is refused, and it will
            say which line.
          </span>
        </label>

        {error && (
          <p className="flex items-start gap-2 rounded-md border border-danger-soft bg-danger-soft p-3 text-sm text-danger">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
            <span>{error}</span>
          </p>
        )}
        {saved && (
          <p className="flex items-start gap-2 rounded-md border border-success-soft bg-success-soft p-3 text-sm text-success">
            <Check className="mt-0.5 h-4 w-4 shrink-0" />
            <span>{saved}</span>
          </p>
        )}

        <div className="flex gap-2">
          <Button
            onClick={save}
            disabled={busy || !id.trim() || !displayName.trim() || !yaml.trim()}
          >
            {busy ? "Checking…" : "Add to catalog"}
          </Button>
          {saved && (
            <Button variant="secondary" onClick={() => navigate(`/add/${id}`)}>
              Install it
            </Button>
          )}
        </div>

        {(apps.data?.length ?? 0) > 0 && (
          <section className="mt-8">
            <h2 className="mb-2 text-sm font-semibold text-fg-muted">
              Your own apps
            </h2>
            <ul className="divide-y divide-border rounded-card border border-border bg-surface">
              {apps.data?.map((a) => (
                <li key={a.id} className="flex items-center gap-3 p-3">
                  <span className="text-lg">{a.icon || "🔧"}</span>
                  <div className="min-w-0 flex-1">
                    <div className="font-medium text-fg">{a.display_name}</div>
                    <p className="truncate text-xs text-fg-subtle">
                      {a.description || a.id}
                    </p>
                  </div>
                  <Button
                    size="sm"
                    variant="secondary"
                    onClick={() => navigate(`/add/${a.id}`)}
                  >
                    Install
                  </Button>
                  <button
                    type="button"
                    onClick={() => remove(a)}
                    disabled={busy}
                    aria-label={`Remove ${a.display_name}`}
                    className="rounded-md p-1.5 text-fg-subtle transition-colors hover:bg-surface-2 hover:text-danger disabled:opacity-50"
                  >
                    <Trash2 className="h-4 w-4" />
                  </button>
                </li>
              ))}
            </ul>
          </section>
        )}

        <p className="flex items-start gap-2 pt-2 text-xs text-fg-muted">
          <FileCode2 className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span>
            Removing an app here only takes it out of the catalog. Anything
            already installed from it keeps running and is uninstalled from its
            own page.
          </span>
        </p>
      </div>
    </Page>
  );
}

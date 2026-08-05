import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import {
  ArrowLeft,
  Check,
  ChevronDown,
  Copy,
  ExternalLink,
  RefreshCw,
  Trash2,
} from "lucide-react";
import { Page } from "@/components/AppShell";
import { AppIcon } from "@/components/AppIcon";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { ConfirmDialog } from "@/components/ui/sheet";
import { Banner, Skeleton, Spinner } from "@/components/ui/feedback";
import { Card } from "@/components/ui/card";
import { api, streamEvents } from "@/lib/api";
import { useResource } from "@/lib/useResource";
import {
  appDisplayName,
  appFacts,
  appLinks,
  appState,
  catalogEntry,
} from "@/lib/apps";
import { taglineFor } from "@/catalog/meta";
import { cn } from "@/lib/utils";
import type {
  AppInfo,
  CatalogApp,
  DomainResponse,
  PodInfo,
  ScanOutputsResponse,
} from "@/types/apps";

function CopyValue({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-center gap-3 px-5 py-4">
      <div className="min-w-0 flex-1">
        <div className="text-sm text-fg-muted">{label}</div>
        <div className="mt-0.5 break-all font-mono text-sm text-fg">
          {value}
        </div>
      </div>
      <button
        onClick={async () => {
          try {
            await navigator.clipboard.writeText(value);
            setCopied(true);
            setTimeout(() => setCopied(false), 1600);
          } catch {
            /* clipboard is blocked outside a secure context; the value is
               still selectable, which is the point of showing it in full */
          }
        }}
        className="shrink-0 rounded-lg p-2.5 text-fg-muted hover:bg-surface-2 hover:text-fg"
        aria-label={`Copy ${label}`}
      >
        {copied ? (
          <Check className="h-4 w-4 text-success" />
        ) : (
          <Copy className="h-4 w-4" />
        )}
      </button>
    </div>
  );
}

/** Pods, logs and the values it was installed with. */
function TechnicalDetails({ app }: { app: AppInfo }) {
  const [open, setOpen] = useState(false);
  const [pods, setPods] = useState<PodInfo[] | null>(null);
  const [logs, setLogs] = useState<{ pod: string; text: string } | null>(null);

  useEffect(() => {
    if (!open || pods) return;
    let cancelled = false;
    void api
      .get<PodInfo[]>(`/api/apps/${app.instance_name}/pods`)
      .then((p) => {
        if (!cancelled) setPods(p);
      })
      .catch(() => {
        if (!cancelled) setPods([]);
      });
    return () => {
      cancelled = true;
    };
  }, [open, pods, app.instance_name]);

  return (
    <div className="mt-6">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex items-center gap-1.5 text-sm text-fg-muted hover:text-fg"
        aria-expanded={open}
      >
        Technical details
        <ChevronDown
          className={cn("h-4 w-4 transition-transform", open && "rotate-180")}
        />
      </button>

      {open && (
        <div className="mt-3 space-y-3">
          {!pods ? (
            <div className="flex justify-center py-4">
              <Spinner />
            </div>
          ) : pods.length > 0 ? (
            <div className="space-y-1.5">
              {pods.map((pod) => (
                <div
                  key={pod.name}
                  className="flex items-center gap-2 rounded-xl bg-surface-2 px-3 py-2"
                >
                  <span
                    className={cn(
                      "h-2 w-2 shrink-0 rounded-full",
                      pod.ready ? "bg-success" : "bg-warning",
                    )}
                  />
                  <span className="min-w-0 flex-1 truncate font-mono text-xs text-fg-muted">
                    {pod.name}
                  </span>
                  <span className="shrink-0 text-xs text-fg-subtle">
                    {pod.phase}
                  </span>
                  <button
                    onClick={async () => {
                      setLogs({ pod: pod.name, text: "" });
                      try {
                        const text = await api.getText(
                          `/api/apps/${app.instance_name}/logs/${pod.name}`,
                        );
                        setLogs({ pod: pod.name, text });
                      } catch (e) {
                        setLogs({
                          pod: pod.name,
                          text: e instanceof Error ? e.message : "No logs",
                        });
                      }
                    }}
                    className="shrink-0 rounded-lg px-2 py-1 text-xs text-primary hover:bg-surface-3"
                  >
                    Logs
                  </button>
                </div>
              ))}
            </div>
          ) : (
            <p className="text-sm text-fg-muted">Nothing running right now.</p>
          )}

          {logs && (
            <pre className="max-h-72 overflow-auto rounded-xl bg-surface-2 p-3 font-mono text-xs leading-relaxed text-fg-muted">
              {logs.text || "…"}
            </pre>
          )}

          {Object.keys(app.config ?? {}).length > 0 && (
            <div className="rounded-xl bg-surface-2 p-4">
              <div className="mb-2 text-xs font-medium text-fg-muted">
                Settings this app was installed with
              </div>
              <dl className="space-y-1">
                {Object.entries(app.config).map(([k, v]) => (
                  <div key={k} className="flex gap-2 text-xs">
                    <dt className="text-fg-subtle">{k}</dt>
                    <dd className="min-w-0 flex-1 break-all font-mono text-fg-muted">
                      {typeof v === "string" ? v : JSON.stringify(v)}
                    </dd>
                  </div>
                ))}
              </dl>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

export function AppDetailPage() {
  const { instanceName } = useParams<{ instanceName: string }>();
  const navigate = useNavigate();

  const apps = useResource<AppInfo[]>("apps", () => api.get("/api/apps"), {
    pollMs: 10_000,
  });
  const catalog = useResource<CatalogApp[]>("catalog", () =>
    api.get("/api/apps/catalog"),
  );
  const domain = useResource<DomainResponse>("domain", () =>
    api.get("/api/tunnel/domain"),
  );

  const [confirmRemove, setConfirmRemove] = useState(false);
  const [working, setWorking] = useState<null | "update" | "remove">(null);
  const [error, setError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);

  const app = apps.data?.find((a) => a.instance_name === instanceName);
  const state = app ? appState(app) : "starting";

  // Depends on `refresh` rather than the whole resource: the resource object is
  // a new identity every render, which would make `scan` — and the effect below
  // that lists it — churn on every poll tick.
  const refreshApps = apps.refresh;
  const scan = useCallback(async () => {
    if (!instanceName) return;
    setScanning(true);
    try {
      await api.post<ScanOutputsResponse>(
        `/api/apps/${instanceName}/scan-outputs`,
      );
      await refreshApps();
    } catch {
      // Scanning reads pod logs; before the pod is up there is nothing to
      // read, and that is a normal state rather than a failure worth showing.
    } finally {
      setScanning(false);
    }
  }, [instanceName, refreshApps]);

  // Scan once automatically when a running app has details it should have but
  // does not. The old UI shipped a "Scan outputs" button and an install
  // message telling people to press it — an internal step that had leaked into
  // the product. Nobody should have to know what an output is.
  const autoScanned = useRef<string | null>(null);
  useEffect(() => {
    if (!app || state !== "ready") return;
    const missing = (app.outputs_spec ?? []).length > 0 && !app.outputs?.length;
    if (!missing || autoScanned.current === app.instance_name) return;
    autoScanned.current = app.instance_name;
    void scan();
  }, [app, state, scan]);

  if (apps.loading) {
    return (
      <Page>
        <Skeleton className="h-32 w-full" />
      </Page>
    );
  }

  if (!app) {
    return (
      <Page title="App not found">
        <p className="text-sm text-fg-muted">
          There is no app called “{instanceName}” on this box. It may have been
          removed.
        </p>
        <Link
          to="/"
          className="mt-4 inline-flex items-center gap-1.5 text-sm text-primary"
        >
          <ArrowLeft className="h-4 w-4" />
          Back to my apps
        </Link>
      </Page>
    );
  }

  const entry = catalogEntry(app, catalog.data ?? []);
  const name = appDisplayName(app, catalog.data ?? []);
  const links = appLinks(app, domain.data?.domain ?? "");
  const facts = appFacts(app);
  const expected = (app.outputs_spec ?? []).length;

  async function remove() {
    if (!app) return;
    setWorking("remove");
    setError(null);
    try {
      await api.del(`/api/apps/${app.instance_name}`);
      navigate("/");
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove the app");
      setWorking(null);
    }
  }

  async function update() {
    if (!app) return;
    setWorking("update");
    setError(null);
    const result = await streamEvents(
      `/api/apps/${app.instance_name}/update`,
      { method: "POST" },
      () => {},
    );
    if (!result.ok) setError(result.error ?? "Could not update the app");
    await apps.refresh();
    setWorking(null);
  }

  return (
    <Page>
      <Link
        to="/"
        className="mb-5 inline-flex items-center gap-1.5 text-sm text-fg-muted hover:text-fg"
      >
        <ArrowLeft className="h-4 w-4" />
        My apps
      </Link>

      <header className="mb-6 flex items-center gap-4">
        <div className="flex h-16 w-16 shrink-0 items-center justify-center rounded-tile bg-surface-2">
          <AppIcon
            icon={entry?.icon ?? ""}
            name={name}
            className="h-9 w-9 text-3xl"
          />
        </div>
        <div className="min-w-0">
          <h1 className="text-2xl font-semibold tracking-tight text-fg">
            {name}
          </h1>
          <p className="mt-0.5 text-sm text-fg-muted">
            {entry
              ? taglineFor({ id: entry.id, description: entry.description })
              : "Installed from a chart no longer in the catalog."}
          </p>
        </div>
      </header>

      {state === "starting" && (
        <Banner tone="info" title="Still starting" className="mb-5">
          This usually takes a minute or two the first time. Details appear here
          as soon as it is up.
        </Banner>
      )}
      {state === "removing" && (
        <Banner tone="warning" title="Being removed" className="mb-5">
          This app and its data are being deleted.
        </Banner>
      )}
      {error && (
        <Banner tone="error" title="That did not work" className="mb-5">
          {error}
        </Banner>
      )}

      {/* Links. An app is not one link — a chart can publish several, and a
          bundle of apps will publish many. They are all offered, rather than
          the first one being treated as "the" address. */}
      {links.length > 0 && (
        <div className="mb-4 space-y-2">
          {links.map((link, i) => (
            <a
              key={link.url}
              href={link.url}
              target="_blank"
              rel="noopener noreferrer"
              className={cn(
                "flex items-center gap-3 rounded-card border p-4 transition-colors",
                i === 0
                  ? "border-primary/20 bg-primary-soft hover:brightness-[0.98]"
                  : "border-border bg-surface hover:bg-surface-2",
              )}
            >
              <div className="min-w-0 flex-1">
                <div
                  className={cn(
                    "text-sm font-medium",
                    i === 0 ? "text-primary" : "text-fg",
                  )}
                >
                  {links.length === 1 ? `Open ${name}` : link.label}
                </div>
                <div className="mt-0.5 truncate font-mono text-xs text-fg-muted">
                  {link.url}
                </div>
              </div>
              <ExternalLink
                className={cn(
                  "h-4 w-4 shrink-0",
                  i === 0 ? "text-primary" : "text-fg-subtle",
                )}
              />
            </a>
          ))}
        </div>
      )}

      {/* Everything that is not a link: a server address to paste into a game,
          an IPv6, a generated credential. For minecraft and valheim this is
          the entire reason to open this page. */}
      {facts.length > 0 && (
        <Card className="mb-4 divide-y divide-border p-0">
          {facts.map((f) => (
            <CopyValue key={f.key} label={f.label} value={f.value} />
          ))}
        </Card>
      )}

      {links.length === 0 && facts.length === 0 && expected > 0 && (
        <Card className="mb-4 p-5">
          <div className="flex items-center gap-3 text-sm text-fg-muted">
            {scanning ? (
              <>
                <Spinner className="h-4 w-4" />
                Looking for this app&rsquo;s details…
              </>
            ) : (
              <>
                <span className="flex-1">
                  This app has not reported its details yet.
                </span>
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={() => void scan()}
                >
                  Look again
                </Button>
              </>
            )}
          </div>
        </Card>
      )}

      {entry && (
        <div className="mb-6 flex flex-wrap items-center gap-2">
          <Badge variant="outline">version {entry.chart_version}</Badge>
          {app.instance_name !== app.app_id && (
            <Badge variant="muted">copy named “{app.instance_name}”</Badge>
          )}
          {entry.repo !== "official" && (
            // A chart from a repo the user added can create arbitrary cluster
            // objects. Saying where this one came from is the minimum.
            <Badge variant="warning">from {entry.repo}</Badge>
          )}
        </div>
      )}

      <div className="flex flex-col gap-2 sm:flex-row">
        <Button
          variant="secondary"
          onClick={() => void update()}
          loading={working === "update"}
          className="flex-1"
        >
          <RefreshCw className="h-4 w-4" />
          Check for updates
        </Button>
        <Button
          variant="quiet"
          onClick={() => setConfirmRemove(true)}
          className="flex-1"
        >
          <Trash2 className="h-4 w-4" />
          Remove
        </Button>
      </div>

      <TechnicalDetails app={app} />

      <ConfirmDialog
        open={confirmRemove}
        onClose={() => setConfirmRemove(false)}
        onConfirm={() => void remove()}
        title={`Remove ${name}?`}
        destructive
        confirmLabel="Remove it"
        busy={working === "remove"}
        body={
          <>
            This deletes {name} and everything stored in it — files, settings
            and history. Backups you have already taken are kept, so this can be
            undone from a backup, but nothing added since the last one will
            survive.
          </>
        }
      />
    </Page>
  );
}

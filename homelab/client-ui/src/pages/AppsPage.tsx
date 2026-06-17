import Form from "@rjsf/core";
import type { WidgetProps } from "@rjsf/utils";
import validator from "@rjsf/validator-ajv8";
import { useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { AlertTriangle } from "lucide-react";
import {
  ArrowLeft,
  Copy,
  Check,
  ExternalLink,
  RefreshCw,
  Trash2,
  ChevronRight,
  RotateCcw,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import type {
  AppInfo,
  CatalogApp,
  DescribeResponse,
  DomainResponse,
  PodInfo,
  ScanOutputsResponse,
} from "@/types/apps";

// ─── Helpers ──────────────────────────────────────────────────────────────────

function statusVariant(status: AppInfo["status"]) {
  if (status === "running") return "success" as const;
  if (status === "uninstalling") return "destructive" as const;
  return "warning" as const;
}

function statusLabel(status: AppInfo["status"]) {
  if (status === "running") return "Running";
  if (status === "uninstalling") return "Uninstalling…";
  return "Starting…";
}

function podColor(pod: PodInfo) {
  if (pod.ready) return "#4ade80";
  if (pod.phase === "Pending" || pod.phase === "Running") return "#fbbf24";
  return "#f87171";
}

function formatConfigValue(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

function AppIcon({ icon }: { icon: string }) {
  if (icon.startsWith("http") || icon.startsWith("/")) {
    return (
      <img
        src={icon}
        alt=""
        className="w-8 h-8 object-contain rounded"
        onError={(e) => {
          (e.target as HTMLImageElement).style.display = "none";
        }}
      />
    );
  }
  return <span className="text-2xl leading-none">{icon}</span>;
}

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  function copy() {
    navigator.clipboard.writeText(text).catch(() => {});
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  }
  return (
    <button
      onClick={copy}
      className="text-[#52525b] hover:text-[#a1a1aa] transition-colors"
      title="Copy"
    >
      {copied ? (
        <Check className="h-3.5 w-3.5 text-[#4ade80]" />
      ) : (
        <Copy className="h-3.5 w-3.5" />
      )}
    </button>
  );
}

function LogLine({ line }: { line: string }) {
  const isError = line.startsWith("[ERROR]");
  return (
    <div
      className={cn(
        "font-mono text-xs leading-5 whitespace-pre-wrap break-all",
        isError ? "text-[#f87171]" : "text-[#86efac]",
      )}
    >
      {line}
    </div>
  );
}

// ─── Form widgets ─────────────────────────────────────────────────────────────

function TunnelWidget({ value, onChange, registry }: WidgetProps) {
  const { tunnelDomain } = registry.formContext as { tunnelDomain: string };
  return (
    <div>
      <input
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value)}
        className="w-full rounded-md border border-[#27272a] bg-[#09090b] px-3 py-2 text-sm text-[#fafafa] outline-none focus:border-[#a78bfa] focus:ring-2 focus:ring-[#a78bfa]/15 transition-colors"
      />
      {tunnelDomain && value && (
        <p className="mt-1.5 text-xs font-mono text-[#a78bfa]">
          https://{value}.{tunnelDomain}
        </p>
      )}
    </div>
  );
}

function PasswordWidget({ value, onChange }: WidgetProps) {
  const [password, setPassword] = useState<string>(value ?? "");
  const [confirm, setConfirm] = useState<string>("");
  const [confirmTouched, setConfirmTouched] = useState(false);
  const showMismatch = confirmTouched && confirm !== "" && password !== confirm;

  function handlePasswordChange(newVal: string) {
    setPassword(newVal);
    onChange(confirm && newVal === confirm ? newVal : "");
  }

  function handleConfirmChange(newVal: string) {
    setConfirmTouched(true);
    setConfirm(newVal);
    onChange(newVal && newVal === password ? password : "");
  }

  return (
    <div className="space-y-2">
      <input
        type="password"
        value={password}
        onChange={(e) => handlePasswordChange(e.target.value)}
        autoComplete="new-password"
        className="w-full rounded-md border border-[#27272a] bg-[#09090b] px-3 py-2 text-sm text-[#fafafa] outline-none focus:border-[#a78bfa] focus:ring-2 focus:ring-[#a78bfa]/15 transition-colors"
        placeholder="Password"
      />
      <input
        type="password"
        value={confirm}
        onChange={(e) => handleConfirmChange(e.target.value)}
        autoComplete="new-password"
        className={cn(
          "w-full rounded-md border px-3 py-2 text-sm text-[#fafafa] outline-none focus:ring-2 transition-colors bg-[#09090b]",
          showMismatch
            ? "border-[#f87171] focus:border-[#f87171] focus:ring-[#f87171]/15"
            : "border-[#27272a] focus:border-[#a78bfa] focus:ring-[#a78bfa]/15",
        )}
        placeholder="Confirm password"
      />
      {showMismatch && (
        <p className="text-xs text-[#f87171]">Passwords do not match</p>
      )}
    </div>
  );
}

// ─── Skeletons ────────────────────────────────────────────────────────────────

function Shimmer({ className }: { className?: string }) {
  return (
    <div className={`animate-pulse rounded bg-[#27272a] ${className ?? ""}`} />
  );
}

function InstalledRowSkeleton() {
  return (
    <Card>
      <CardContent className="flex items-center justify-between gap-4 py-4">
        <div className="space-y-2">
          <div className="flex items-center gap-2">
            <Shimmer className="h-3.5 w-28" />
            <Shimmer className="h-5 w-16 rounded-full" />
          </div>
          <Shimmer className="h-3 w-40" />
        </div>
        <Shimmer className="h-4 w-4 flex-shrink-0" />
      </CardContent>
    </Card>
  );
}

function CatalogCardSkeleton() {
  return (
    <Card>
      <CardContent className="pt-5 pb-4">
        <div className="flex items-start gap-3">
          <Shimmer className="flex-shrink-0 w-9 h-9 rounded-md" />
          <div className="flex-1 space-y-2">
            <Shimmer className="h-3.5 w-24" />
            <Shimmer className="h-3 w-full" />
            <Shimmer className="h-3 w-3/4" />
            <Shimmer className="h-2.5 w-14 mt-1" />
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ─── Main apps list ───────────────────────────────────────────────────────────

const APPS_CACHE_KEY = "yolab:installed_apps";

export function AppsPage() {
  const navigate = useNavigate();
  const [catalog, setCatalog] = useState<CatalogApp[]>([]);
  const [installed, setInstalled] = useState<AppInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [stale, setStale] = useState(false);

  function loadInstalled() {
    return fetch("/api/apps")
      .then((r) => r.json())
      .then((a: AppInfo[]) => {
        if (a.length > 0) {
          localStorage.setItem(APPS_CACHE_KEY, JSON.stringify(a));
          setInstalled(a);
          setStale(false);
        } else {
          // Empty — K3s may be unreachable, keep cached data
          setInstalled((prev) => {
            if (prev.length > 0) { setStale(true); return prev; }
            return a;
          });
        }
      })
      .catch(() => {
        setStale(true);
      });
  }

  useEffect(() => {
    try {
      const cached = localStorage.getItem(APPS_CACHE_KEY);
      if (cached) {
        setInstalled(JSON.parse(cached) as AppInfo[]);
        setStale(true);
      }
    } catch {}

    const catalogP = fetch("/api/apps/catalog")
      .then((r) => r.json())
      .then((c) => setCatalog(c as CatalogApp[]))
      .catch(() => {});
    void Promise.all([catalogP, loadInstalled()]).finally(() =>
      setLoading(false),
    );
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    const needsRefresh = installed.some((a) => a.status !== "running");
    if (!needsRefresh) return;
    const id = setInterval(() => void loadInstalled(), 3000);
    return () => clearInterval(id);
  }, [installed]); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <div className="space-y-8 max-w-4xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Apps</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Manage and install self-hosted apps
        </p>
      </div>

      {stale && (
        <div className="flex items-start gap-2.5 rounded-lg border border-[#fbbf24]/30 bg-[#fbbf24]/5 px-4 py-3">
          <AlertTriangle className="h-4 w-4 text-[#fbbf24] mt-0.5 flex-shrink-0" />
          <p className="text-sm text-[#fbbf24]">
            Cluster API unreachable — showing last known apps. They may still be running but status is unknown.
          </p>
        </div>
      )}

      {/* Installed */}
      {loading ? (
        <section className="space-y-3">
          <Shimmer className="h-3 w-16" />
          <div className="space-y-2">
            {[1, 2].map((i) => (
              <InstalledRowSkeleton key={i} />
            ))}
          </div>
        </section>
      ) : (
        installed.length > 0 && (
          <section className="space-y-3">
            <h2 className="text-xs font-semibold uppercase tracking-wider text-[#52525b]">
              Installed
            </h2>
            <div className="space-y-2">
              {installed.map((app) => {
                const primaryOutput = app.outputs.find(
                  (o) => o.type !== "hidden",
                );
                return (
                  <Card
                    key={app.instance_name}
                    className="cursor-pointer hover:bg-[#1f1f23] transition-colors"
                    onClick={() => navigate(`/installed/${app.instance_name}`)}
                  >
                    <CardContent className="flex items-center justify-between gap-4 py-4">
                      <div className="flex items-center gap-3 min-w-0">
                        <div>
                          <div className="flex items-center gap-2">
                            <span className="text-sm font-medium text-[#fafafa]">
                              {app.app_id}
                            </span>
                            <Badge variant={statusVariant(app.status)}>
                              {statusLabel(app.status)}
                            </Badge>
                          </div>
                          <span className="text-xs text-[#52525b] font-mono">
                            {app.instance_name}
                          </span>
                        </div>
                      </div>
                      <div className="flex items-center gap-3 flex-shrink-0">
                        {primaryOutput &&
                          (primaryOutput.type === "url" ? (
                            <a
                              href={primaryOutput.value}
                              target="_blank"
                              rel="noreferrer"
                              onClick={(e) => e.stopPropagation()}
                              className="text-xs font-mono text-[#a78bfa] hover:text-[#c4b5fd] flex items-center gap-1 transition-colors max-w-[200px] truncate"
                            >
                              {primaryOutput.value}
                              <ExternalLink className="h-3 w-3 flex-shrink-0" />
                            </a>
                          ) : (
                            <span
                              onClick={(e) => e.stopPropagation()}
                              className="text-xs font-mono text-[#71717a] max-w-[200px] truncate"
                            >
                              {primaryOutput.value}
                            </span>
                          ))}
                        <ChevronRight className="h-4 w-4 text-[#52525b]" />
                      </div>
                    </CardContent>
                  </Card>
                );
              })}
            </div>
          </section>
        )
      )}

      {/* Catalog */}
      <section className="space-y-3">
        <h2 className="text-xs font-semibold uppercase tracking-wider text-[#52525b]">
          Available
        </h2>
        {loading ? (
          <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3">
            {[1, 2, 3, 4, 5, 6].map((i) => (
              <CatalogCardSkeleton key={i} />
            ))}
          </div>
        ) : catalog.length === 0 ? (
          <p className="text-sm text-[#71717a]">No apps available.</p>
        ) : (
          <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3">
            {catalog.map((app) => (
              <Card
                key={app.id}
                className="cursor-pointer hover:bg-[#1f1f23] hover:border-[#3f3f46] transition-all"
                onClick={() => navigate(`/apps/${app.id}`)}
              >
                <CardContent className="pt-5 pb-4">
                  <div className="flex items-start gap-3">
                    <div className="flex-shrink-0 w-9 h-9 flex items-center justify-center">
                      <AppIcon icon={app.icon} />
                    </div>
                    <div className="min-w-0">
                      <p className="text-sm font-medium text-[#fafafa] leading-tight">
                        {app.name}
                      </p>
                      <p className="text-xs text-[#71717a] mt-1 leading-relaxed line-clamp-2">
                        {app.description}
                      </p>
                      <span className="inline-block mt-2 text-[10px] uppercase tracking-wider text-[#52525b]">
                        {app.category}
                      </span>
                    </div>
                  </div>
                </CardContent>
              </Card>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}

// ─── Install page ─────────────────────────────────────────────────────────────

export function AppInstallPage() {
  const { appId } = useParams<{ appId: string }>();
  const navigate = useNavigate();
  const [app, setApp] = useState<CatalogApp | null>(null);
  const [instanceName, setInstanceName] = useState(appId ?? "");
  const [formData, setFormData] = useState<object>({});
  const [tunnelDomain, setTunnelDomain] = useState("");
  const [log, setLog] = useState<string[]>([]);
  const [installing, setInstalling] = useState(false);
  const [done, setDone] = useState(false);
  const [error, setError] = useState("");
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    fetch("/api/apps/catalog")
      .then((r) => r.json())
      .then((catalog: CatalogApp[]) => {
        const found = catalog.find((a) => a.id === appId);
        if (found) setApp(found);
      })
      .catch(() => {});
    fetch("/api/tunnel/domain")
      .then((r) => r.json())
      .then((d: DomainResponse) => setTunnelDomain(d.domain))
      .catch(() => {});
  }, [appId]);

  useEffect(() => {
    if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight;
  }, [log]);

  async function install() {
    setInstalling(true);
    setLog([]);
    setError("");
    const response = await fetch(`/api/apps/${appId}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ instance_name: instanceName, config: formData }),
    });
    if (!response.body) {
      setInstalling(false);
      return;
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    while (true) {
      const { done: streamDone, value } = await reader.read();
      if (streamDone) break;
      buf += decoder.decode(value, { stream: true });
      const parts = buf.split("\n\n");
      buf = parts.pop() ?? "";
      for (const part of parts) {
        const line = part.startsWith("data: ") ? part.slice(6) : part;
        if (!line.trim()) continue;
        if (line.startsWith("[ERROR]")) setError(line);
        if (line.startsWith("[DONE]")) setDone(true);
        setLog((l) => [...l, line]);
      }
    }
    setInstalling(false);
  }

  if (!app) return <p className="text-sm text-[#71717a]">Loading…</p>;

  return (
    <div className="max-w-xl space-y-6">
      <button
        onClick={() => navigate("/apps")}
        className="flex items-center gap-1.5 text-sm text-[#71717a] hover:text-[#fafafa] transition-colors"
      >
        <ArrowLeft className="h-4 w-4" />
        Back to Apps
      </button>

      {/* App header */}
      <div className="flex items-center gap-4">
        <div className="w-12 h-12 flex items-center justify-center rounded-xl border border-[#27272a] bg-[#18181b]">
          <AppIcon icon={app.icon} />
        </div>
        <div>
          <h1 className="text-xl font-semibold text-[#fafafa]">{app.name}</h1>
          <p className="text-sm text-[#71717a] mt-0.5">{app.description}</p>
        </div>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Configuration</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          <div>
            <label className="block text-xs font-medium text-[#a1a1aa] mb-1.5">
              Instance name
            </label>
            <input
              value={instanceName}
              onChange={(e) => setInstanceName(e.target.value)}
              className="w-full rounded-md border border-[#27272a] bg-[#09090b] px-3 py-2 text-sm text-[#fafafa] outline-none focus:border-[#a78bfa] focus:ring-2 focus:ring-[#a78bfa]/15 transition-colors"
            />
          </div>

          <Form
            className="yolab-form"
            schema={app.schema as never}
            uiSchema={app.uischema as never}
            validator={validator}
            formData={formData}
            onChange={({ formData: d }) => setFormData(d ?? {})}
            onSubmit={() => void install()}
            widgets={{ TunnelWidget, PasswordWidget }}
            formContext={{ tunnelDomain }}
          >
            <Button
              type="submit"
              disabled={installing || done}
              className={cn(
                "w-full mt-2",
                done && "bg-[#4ade80] hover:bg-[#4ade80] text-[#09090b]",
                installing && "opacity-70",
              )}
            >
              {done ? (
                <>
                  <Check className="h-4 w-4" />
                  Installed
                </>
              ) : installing ? (
                <>
                  <RefreshCw className="h-4 w-4 animate-spin" />
                  Installing…
                </>
              ) : (
                "Install"
              )}
            </Button>
          </Form>
        </CardContent>
      </Card>

      {log.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle>Install log</CardTitle>
          </CardHeader>
          <CardContent>
            <div
              ref={logRef}
              className="rounded-lg bg-[#09090b] border border-[#27272a] p-3 max-h-64 overflow-y-auto space-y-0.5"
            >
              {log.map((l, i) => (
                <LogLine key={i} line={l} />
              ))}
            </div>
          </CardContent>
        </Card>
      )}

      {error && <p className="text-sm text-[#f87171]">{error}</p>}

      {done && (
        <Button variant="outline" onClick={() => navigate("/apps")}>
          <ArrowLeft className="h-4 w-4" />
          Back to Apps
        </Button>
      )}
    </div>
  );
}

// ─── Installed detail page ────────────────────────────────────────────────────

export function InstalledDetailPage() {
  const { instanceName } = useParams<{ instanceName: string }>();
  const navigate = useNavigate();
  const [app, setApp] = useState<AppInfo | null>(null);
  const [pods, setPods] = useState<PodInfo[]>([]);
  const [selectedPod, setSelectedPod] = useState<string | null>(null);
  const [view, setView] = useState<"describe" | "logs">("describe");
  const [describe, setDescribe] = useState("");
  const [logs, setLogs] = useState<string[]>([]);
  const [scanning, setScanning] = useState(false);
  const [updating, setUpdating] = useState(false);
  const [updateLog, setUpdateLog] = useState<string[]>([]);
  const [confirming, setConfirming] = useState(false);
  const [uninstalling, setUninstalling] = useState(false);
  const [uninstallError, setUninstallError] = useState("");
  const [reconfiguring, setReconfiguring] = useState(false);
  const [reconfData, setReconfData] = useState<object>({});
  const [catalogApp, setCatalogApp] = useState<CatalogApp | null>(null);
  const outputRef = useRef<HTMLDivElement>(null);
  const readerRef = useRef<ReadableStreamDefaultReader | null>(null);

  function loadApp() {
    fetch("/api/apps")
      .then((r) => r.json())
      .then((apps: AppInfo[]) => {
        const found = apps.find((a) => a.instance_name === instanceName);
        if (found) {
          setApp(found);
          setReconfData(found.config as object);
        }
      })
      .catch(() => {});
  }

  function loadPods() {
    fetch(`/api/apps/${instanceName}/pods`)
      .then((r) => r.json())
      .then((p) => setPods(p as PodInfo[]))
      .catch(() => {});
  }

  function scanOutputs(): Promise<void> {
    return fetch(`/api/apps/${instanceName}/scan-outputs`, { method: "POST" })
      .then((r) => r.json())
      .then((data: ScanOutputsResponse) =>
        setApp((prev) => (prev ? { ...prev, outputs: data.outputs } : prev)),
      )
      .catch(() => {});
  }

  useEffect(() => {
    loadApp();
    loadPods();
    void scanOutputs();
    const id = setInterval(() => {
      loadApp();
      loadPods();
    }, 3000);
    return () => clearInterval(id);
  }, [instanceName]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!app) return;
    fetch("/api/apps/catalog")
      .then((r) => r.json())
      .then((c: CatalogApp[]) => {
        const found = c.find((a) => a.id === app.app_id);
        if (found) setCatalogApp(found);
      })
      .catch(() => {});
  }, [app?.app_id]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (outputRef.current)
      outputRef.current.scrollTop = outputRef.current.scrollHeight;
  }, [logs, describe]);

  useEffect(
    () => () => {
      readerRef.current?.cancel();
    },
    [],
  );

  async function selectPod(podName: string) {
    readerRef.current?.cancel();
    readerRef.current = null;
    setSelectedPod(podName);
    setView("describe");
    setDescribe("");
    setLogs([]);
    const r = await fetch(
      `/api/apps/${instanceName}/describe/${podName}`,
    ).catch(() => null);
    if (r?.ok) setDescribe(((await r.json()) as DescribeResponse).output);
  }

  async function showLogs(podName: string) {
    readerRef.current?.cancel();
    readerRef.current = null;
    setView("logs");
    setLogs([]);
    const r = await fetch(`/api/apps/${instanceName}/logs/${podName}`);
    if (!r.body) return;
    const reader = r.body.getReader();
    readerRef.current = reader;
    const decoder = new TextDecoder();
    let buf = "";
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      const parts = buf.split("\n\n");
      buf = parts.pop() ?? "";
      for (const part of parts) {
        const line = part.startsWith("data: ") ? part.slice(6) : part;
        if (line.trim()) setLogs((l) => [...l, line]);
      }
    }
  }

  async function doUpdate() {
    setUpdating(true);
    setUpdateLog([]);
    const response = await fetch(`/api/apps/${instanceName}/update`, {
      method: "POST",
    });
    if (!response.body) {
      setUpdating(false);
      return;
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      const parts = buf.split("\n\n");
      buf = parts.pop() ?? "";
      for (const part of parts) {
        const line = part.startsWith("data: ") ? part.slice(6) : part;
        if (line.trim()) setUpdateLog((l) => [...l, line]);
      }
    }
    setUpdating(false);
  }

  async function doUninstall() {
    if (!confirming) {
      setConfirming(true);
      return;
    }
    setUninstalling(true);
    setUninstallError("");
    const r = await fetch(`/api/apps/${instanceName}`, { method: "DELETE" });
    if (r.ok) navigate("/apps");
    else {
      const d = (await r.json().catch(() => ({}))) as { detail?: string };
      setUninstallError(d.detail ?? "Uninstall failed");
      setUninstalling(false);
      setConfirming(false);
    }
  }

  async function doReconfigure() {
    setReconfiguring(true);
    setUpdateLog([]);
    const response = await fetch(`/api/apps/${instanceName}/update`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ config: reconfData }),
    });
    if (!response.body) {
      setReconfiguring(false);
      return;
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      const parts = buf.split("\n\n");
      buf = parts.pop() ?? "";
      for (const part of parts) {
        const line = part.startsWith("data: ") ? part.slice(6) : part;
        if (line.trim()) setUpdateLog((l) => [...l, line]);
      }
    }
    setReconfiguring(false);
  }

  if (!app)
    return (
      <div className="space-y-4">
        <button
          onClick={() => navigate("/apps")}
          className="flex items-center gap-1.5 text-sm text-[#71717a] hover:text-[#fafafa] transition-colors"
        >
          <ArrowLeft className="h-4 w-4" />
          Back to Apps
        </button>
        <p className="text-sm text-[#71717a]">Loading…</p>
      </div>
    );

  const configEntries = Object.entries(app.config).filter(
    ([, v]) => v !== null && v !== undefined && v !== "",
  );

  return (
    <div className="space-y-6 max-w-3xl">
      <button
        onClick={() => navigate("/apps")}
        className="flex items-center gap-1.5 text-sm text-[#71717a] hover:text-[#fafafa] transition-colors"
      >
        <ArrowLeft className="h-4 w-4" />
        Back to Apps
      </button>

      {/* Header */}
      <div className="flex items-start justify-between gap-4 flex-wrap">
        <div>
          <div className="flex items-center gap-2.5">
            <h1 className="text-xl font-semibold text-[#fafafa]">
              {app.app_id}
            </h1>
            <Badge variant={statusVariant(app.status)}>
              {statusLabel(app.status)}
            </Badge>
          </div>
          <p className="text-sm font-mono text-[#52525b] mt-0.5">
            {instanceName}
          </p>
          {uninstallError && (
            <p className="text-xs text-[#f87171] mt-1">{uninstallError}</p>
          )}
        </div>

        <div className="flex items-center gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => void doUpdate()}
            disabled={updating || uninstalling || app.status === "uninstalling"}
          >
            <RotateCcw
              className={cn("h-3.5 w-3.5", updating && "animate-spin")}
            />
            {updating ? "Updating…" : "Update"}
          </Button>
          <Button
            variant="destructive"
            size="sm"
            onClick={() => void doUninstall()}
            disabled={uninstalling || updating || app.status === "uninstalling"}
          >
            <Trash2 className="h-3.5 w-3.5" />
            {uninstalling
              ? "Removing…"
              : confirming
                ? "Confirm uninstall"
                : "Uninstall"}
          </Button>
        </div>
      </div>

      {updateLog.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle>Update log</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="rounded-lg bg-[#09090b] border border-[#27272a] p-3 max-h-48 overflow-y-auto space-y-0.5">
              {updateLog.map((l, i) => (
                <LogLine key={i} line={l} />
              ))}
            </div>
          </CardContent>
        </Card>
      )}

      {/* Outputs */}
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <CardTitle>Outputs</CardTitle>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                setScanning(true);
                scanOutputs().finally(() => setScanning(false));
              }}
              disabled={scanning}
              className="h-7 text-xs gap-1.5"
            >
              <RefreshCw
                className={cn("h-3 w-3", scanning && "animate-spin")}
              />
              {scanning ? "Scanning…" : "Scan"}
            </Button>
          </div>
        </CardHeader>
        <CardContent>
          {app.outputs_spec.length === 0 ? (
            <p className="text-xs text-[#71717a]">No outputs defined.</p>
          ) : (
            <div className="space-y-2">
              {app.outputs_spec.map((spec) => {
                const scanned = app.outputs.find((o) => o.key === spec.key);
                return (
                  <div
                    key={spec.key}
                    className="flex items-center gap-3 min-w-0"
                  >
                    <span className="text-xs text-[#71717a] w-20 flex-shrink-0">
                      {spec.label}
                    </span>
                    {scanned ? (
                      <div className="flex items-center gap-2 min-w-0">
                        {spec.type === "url" ? (
                          <a
                            href={scanned.value}
                            target="_blank"
                            rel="noreferrer"
                            className="text-xs font-mono text-[#a78bfa] hover:text-[#c4b5fd] flex items-center gap-1 transition-colors truncate"
                          >
                            {scanned.value}
                            <ExternalLink className="h-3 w-3 flex-shrink-0" />
                          </a>
                        ) : (
                          <span className="text-xs font-mono text-[#e4e4e7] truncate">
                            {scanned.value}
                          </span>
                        )}
                        <CopyButton text={scanned.value} />
                      </div>
                    ) : (
                      <span className="text-xs text-[#3f3f46]">
                        {scanning ? "Scanning…" : "Not found"}
                      </span>
                    )}
                  </div>
                );
              })}
            </div>
          )}
        </CardContent>
      </Card>

      {/* Config */}
      {configEntries.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle>Configuration</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-sm">
              {configEntries.map(([k, v]) => (
                <>
                  <span
                    key={`k-${k}`}
                    className="text-xs text-[#71717a] whitespace-nowrap pt-0.5"
                  >
                    {k}
                  </span>
                  <span
                    key={`v-${k}`}
                    className="text-xs font-mono text-[#e4e4e7] break-all"
                  >
                    {formatConfigValue(v)}
                  </span>
                </>
              ))}
            </div>
          </CardContent>
        </Card>
      )}

      {/* Reconfigure */}
      {catalogApp && (
        <Card>
          <CardHeader>
            <CardTitle>Reconfigure</CardTitle>
          </CardHeader>
          <CardContent>
            <Form
              className="yolab-form"
              schema={catalogApp.schema as never}
              uiSchema={catalogApp.uischema as never}
              validator={validator}
              formData={reconfData}
              onChange={({ formData: d }) => setReconfData(d ?? {})}
              onSubmit={() => void doReconfigure()}
              widgets={{ TunnelWidget, PasswordWidget }}
              formContext={{ tunnelDomain: "" }}
            >
              <Button
                type="submit"
                variant="outline"
                size="sm"
                disabled={reconfiguring || app.status === "uninstalling"}
                className="mt-2"
              >
                {reconfiguring ? (
                  <>
                    <RefreshCw className="h-3.5 w-3.5 animate-spin" />
                    Applying…
                  </>
                ) : (
                  "Apply"
                )}
              </Button>
            </Form>
          </CardContent>
        </Card>
      )}

      {/* Pods */}
      <Card>
        <CardHeader>
          <CardTitle>Pods</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap gap-2">
            {pods.length === 0 ? (
              <p className="text-xs text-[#71717a]">No pods found.</p>
            ) : (
              pods.map((pod) => (
                <button
                  key={pod.name}
                  onClick={() => void selectPod(pod.name)}
                  className={cn(
                    "flex items-center gap-2 px-3 py-1.5 rounded-md text-xs font-mono transition-all",
                    selectedPod === pod.name
                      ? "bg-[#27272a] border border-[#3f3f46] text-[#fafafa]"
                      : "border border-[#27272a] text-[#a1a1aa] hover:bg-[#18181b] hover:text-[#fafafa]",
                  )}
                >
                  <span
                    className="w-2 h-2 rounded-full flex-shrink-0"
                    style={{ background: podColor(pod) }}
                  />
                  {pod.name}
                </button>
              ))
            )}
          </div>

          {selectedPod && (
            <div className="space-y-2">
              <div className="flex gap-1">
                {(["describe", "logs"] as const).map((v) => (
                  <button
                    key={v}
                    onClick={() =>
                      v === "logs"
                        ? void showLogs(selectedPod)
                        : setView("describe")
                    }
                    className={cn(
                      "px-3 py-1 rounded-md text-xs transition-colors",
                      view === v
                        ? "bg-[#27272a] text-[#fafafa]"
                        : "text-[#71717a] hover:text-[#fafafa] hover:bg-[#18181b]",
                    )}
                  >
                    {v}
                  </button>
                ))}
              </div>
              <div
                ref={outputRef}
                className="rounded-lg bg-[#09090b] border border-[#27272a] p-3 min-h-48 max-h-[400px] overflow-y-auto font-mono text-xs text-[#a1a1aa] whitespace-pre-wrap"
              >
                {view === "describe" ? (
                  describe || <span className="text-[#52525b]">Loading…</span>
                ) : logs.length === 0 ? (
                  <span className="text-[#52525b]">Waiting for logs…</span>
                ) : (
                  logs.map((l, i) => <div key={i}>{l}</div>)
                )}
              </div>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

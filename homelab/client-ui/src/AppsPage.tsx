import Form from "@rjsf/core";
import type { FieldProps, WidgetProps } from "@rjsf/utils";
import validator from "@rjsf/validator-ajv8";
import { useEffect, useRef, useState } from "react";

// ─── Types ───────────────────────────────────────────────────────────────────

interface CatalogApp {
  id: string;
  name: string;
  description: string;
  icon: string;
  category: string;
  schema: object;
  uischema: object;
}

interface AppOutput {
  key: string;
  label: string;
  value: string;
  type: "url" | "text" | "hidden";
}

interface InstalledApp {
  app_id: string;
  instance_name: string;
  status: "starting" | "running" | "uninstalling";
  outputs: AppOutput[];
  config: Record<string, unknown>;
}

interface Pod {
  name: string;
  phase: string;
  ready: boolean;
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

function StatusBadge({ status }: { status: InstalledApp["status"] }) {
  const map = {
    running: { label: "Running", color: "#22c55e", bg: "#f0fdf4" },
    starting: { label: "Starting…", color: "#f59e0b", bg: "#fffbeb" },
    uninstalling: { label: "Uninstalling…", color: "#ef4444", bg: "#fef2f2" },
  };
  const s = map[status] ?? map.starting;
  return (
    <span
      style={{
        fontSize: "0.7rem",
        padding: "0.15rem 0.55rem",
        borderRadius: 99,
        background: s.bg,
        color: s.color,
        fontWeight: 600,
        whiteSpace: "nowrap",
      }}
    >
      {s.label}
    </span>
  );
}

function SectionHeader({ title }: { title: string }) {
  return (
    <div
      style={{
        fontSize: "0.72rem",
        fontWeight: 700,
        color: "#999",
        letterSpacing: "0.08em",
        textTransform: "uppercase",
        marginBottom: "0.6rem",
        marginTop: "1.5rem",
        paddingBottom: "0.3rem",
        borderBottom: "1px solid #f0f0f0",
      }}
    >
      {title}
    </div>
  );
}

function BackButton({ onClick }: { onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      style={{
        background: "none",
        border: "none",
        cursor: "pointer",
        fontSize: "0.82rem",
        color: "#666",
        fontFamily: "monospace",
        padding: 0,
        marginBottom: "1.5rem",
        display: "flex",
        alignItems: "center",
        gap: "0.3rem",
      }}
    >
      ← Back to Apps
    </button>
  );
}

function copyText(text: string) {
  navigator.clipboard.writeText(text).catch(() => {});
}

function phaseColor(pod: Pod) {
  if (pod.ready) return "#22c55e";
  if (pod.phase === "Pending" || pod.phase === "Running") return "#f59e0b";
  return "#ef4444";
}

function formatConfigValue(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "object") {
    const o = value as Record<string, string>;
    if (o.host && o.path) return `${o.host} — ${o.path}`;
    return JSON.stringify(value);
  }
  return String(value);
}

// ─── Form widgets ─────────────────────────────────────────────────────────────

function TunnelWidget({ value, onChange, registry }: WidgetProps) {
  const { tunnelDomain } = registry.formContext as { tunnelDomain: string };
  return (
    <div>
      <input
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value)}
        style={{
          width: "100%",
          border: "1px solid #e5e7eb",
          borderRadius: 6,
          padding: "0.4rem 0.6rem",
          fontSize: "0.9rem",
          boxSizing: "border-box",
        }}
      />
      {tunnelDomain && value && (
        <div
          style={{
            fontSize: "0.75rem",
            color: "#7c3aed",
            marginTop: 4,
            fontFamily: "monospace",
          }}
        >
          {`https://${value}.${tunnelDomain}`}
        </div>
      )}
    </div>
  );
}

interface StorageLocation {
  host: string;
  path: string;
}

function DiskField({ formData, onChange }: FieldProps) {
  const [locations, setLocations] = useState<StorageLocation[] | null>(null);
  useEffect(() => {
    fetch("/api/storage")
      .then((r) => r.json())
      .then(setLocations)
      .catch(() => setLocations([]));
  }, []);
  if (locations === null)
    return <div style={{ fontSize: "0.82rem", color: "#999" }}>Loading…</div>;
  if (locations.length === 0)
    return (
      <div style={{ fontSize: "0.82rem", color: "#ef4444" }}>
        No storage available. Export a disk as NFS on the Disks page first.
      </div>
    );
  const currentKey = formData?.host
    ? JSON.stringify({ host: formData.host, path: formData.path })
    : "";
  return (
    <div>
      <label
        style={{
          display: "block",
          fontSize: "0.78rem",
          fontWeight: "bold",
          marginBottom: 4,
        }}
      >
        Storage
      </label>
      <select
        value={currentKey}
        onChange={(e) => {
          if (e.target.value) onChange(JSON.parse(e.target.value));
        }}
        style={{
          width: "100%",
          border: "1px solid #e5e7eb",
          borderRadius: 6,
          padding: "0.4rem 0.6rem",
          fontSize: "0.9rem",
        }}
      >
        <option value="">Select storage…</option>
        {locations.map((loc) => {
          const key = JSON.stringify({ host: loc.host, path: loc.path });
          return (
            <option key={key} value={key}>
              {loc.host} — {loc.path}
            </option>
          );
        })}
      </select>
    </div>
  );
}

// ─── Main apps page (catalog + installed list) ────────────────────────────────

function MainAppsPage({ navigate }: { navigate: (to: string) => void }) {
  const [catalog, setCatalog] = useState<CatalogApp[]>([]);
  const [installed, setInstalled] = useState<InstalledApp[]>([]);

  function loadInstalled() {
    fetch("/api/apps")
      .then((r) => r.json())
      .then(setInstalled)
      .catch(() => {});
  }

  useEffect(() => {
    fetch("/api/apps/catalog")
      .then((r) => r.json())
      .then(setCatalog)
      .catch(() => {});
    loadInstalled();
  }, []);

  useEffect(() => {
    const needsRefresh = installed.some((a) => a.status !== "running");
    if (!needsRefresh) return;
    const id = setInterval(loadInstalled, 3000);
    return () => clearInterval(id);
  }, [installed]);

  return (
    <div>
      {installed.length > 0 && (
        <div style={{ marginBottom: "2.5rem" }}>
          <SectionHeader title="Installed" />
          <div
            style={{ display: "flex", flexDirection: "column", gap: "0.4rem" }}
          >
            {installed.map((app) => {
              const primaryOutput = app.outputs.find((o) => o.type === "url");
              return (
                <div
                  key={app.instance_name}
                  onClick={() => navigate(`/installed/${app.instance_name}`)}
                  style={{
                    border: "1px solid #e5e7eb",
                    borderRadius: 8,
                    padding: "0.9rem 1.1rem",
                    cursor: "pointer",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "1rem",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "0.6rem",
                    }}
                  >
                    <div>
                      <div style={{ fontWeight: "bold", fontSize: "0.95rem" }}>
                        {app.app_id}
                      </div>
                      <div
                        style={{
                          fontSize: "0.78rem",
                          color: "#888",
                          marginTop: 1,
                        }}
                      >
                        {app.instance_name}
                      </div>
                    </div>
                    <StatusBadge status={app.status} />
                  </div>
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "1rem",
                    }}
                  >
                    {primaryOutput && (
                      <a
                        href={primaryOutput.value}
                        target="_blank"
                        rel="noreferrer"
                        onClick={(e) => e.stopPropagation()}
                        style={{
                          fontSize: "0.8rem",
                          color: "#7c3aed",
                          fontFamily: "monospace",
                        }}
                      >
                        {primaryOutput.value}
                      </a>
                    )}
                    <span style={{ fontSize: "0.78rem", color: "#bbb" }}>
                      →
                    </span>
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      )}

      <SectionHeader title="Available" />
      <div
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(auto-fill, minmax(200px, 1fr))",
          gap: "0.75rem",
        }}
      >
        {catalog.map((app) => (
          <div
            key={app.id}
            onClick={() => navigate(`/apps/${app.id}`)}
            style={{
              border: "1px solid #e5e7eb",
              borderRadius: 8,
              padding: "1.1rem",
              cursor: "pointer",
              transition: "border-color 0.15s",
            }}
            onMouseEnter={(e) =>
              (e.currentTarget.style.borderColor = "#1a1a1a")
            }
            onMouseLeave={(e) =>
              (e.currentTarget.style.borderColor = "#e5e7eb")
            }
          >
            <div style={{ fontSize: "1.8rem", marginBottom: "0.4rem" }}>
              {app.icon}
            </div>
            <div
              style={{
                fontWeight: "bold",
                fontSize: "0.95rem",
                marginBottom: "0.2rem",
              }}
            >
              {app.name}
            </div>
            <div style={{ fontSize: "0.78rem", color: "#666" }}>
              {app.description}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

// ─── App install page ─────────────────────────────────────────────────────────

function AppInstallPage({
  appId,
  navigate,
}: {
  appId: string;
  navigate: (to: string) => void;
}) {
  const [app, setApp] = useState<CatalogApp | null>(null);
  const [instanceName, setInstanceName] = useState(appId);
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
      .then((d) => setTunnelDomain(d.domain))
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

  if (!app)
    return <div style={{ color: "#999", fontSize: "0.85rem" }}>Loading…</div>;

  return (
    <div style={{ maxWidth: 540 }}>
      <BackButton onClick={() => navigate("/apps")} />

      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: "0.75rem",
          marginBottom: "1.5rem",
        }}
      >
        <span style={{ fontSize: "2.2rem" }}>{app.icon}</span>
        <div>
          <h2 style={{ margin: 0, fontSize: "1.3rem" }}>{app.name}</h2>
          <div style={{ fontSize: "0.83rem", color: "#666", marginTop: 2 }}>
            {app.description}
          </div>
        </div>
      </div>

      <SectionHeader title="Install" />

      <div style={{ marginBottom: "1rem" }}>
        <label
          style={{
            display: "block",
            fontSize: "0.78rem",
            fontWeight: "bold",
            marginBottom: 4,
          }}
        >
          Instance name
        </label>
        <input
          value={instanceName}
          onChange={(e) => setInstanceName(e.target.value)}
          style={{
            width: "100%",
            border: "1px solid #e5e7eb",
            borderRadius: 6,
            padding: "0.4rem 0.6rem",
            fontSize: "0.9rem",
            boxSizing: "border-box",
          }}
        />
      </div>

      <Form
        schema={app.schema as never}
        uiSchema={app.uischema as never}
        validator={validator}
        formData={formData}
        onChange={({ formData: d }) => setFormData(d ?? {})}
        onSubmit={() => install()}
        widgets={{ TunnelWidget }}
        fields={{ DiskField } as never}
        formContext={{ tunnelDomain }}
      >
        <button
          type="submit"
          disabled={installing || done}
          style={{
            width: "100%",
            padding: "0.65rem",
            background: done ? "#22c55e" : installing ? "#999" : "#1a1a1a",
            color: "#fff",
            border: "none",
            borderRadius: 6,
            cursor: installing || done ? "not-allowed" : "pointer",
            fontWeight: "bold",
            marginTop: "0.5rem",
          }}
        >
          {done ? "Installed!" : installing ? "Installing…" : "Install"}
        </button>
      </Form>

      {log.length > 0 && (
        <>
          <SectionHeader title="Install log" />
          <div
            ref={logRef}
            style={{
              background: "#111",
              color: "#d1fae5",
              borderRadius: 6,
              padding: "0.75rem",
              fontSize: "0.75rem",
              maxHeight: 250,
              overflowY: "auto",
              fontFamily: "monospace",
              whiteSpace: "pre-wrap",
            }}
          >
            {log.map((l, i) => (
              <div
                key={i}
                style={{
                  color: l.startsWith("[ERROR]") ? "#f87171" : "#d1fae5",
                }}
              >
                {l}
              </div>
            ))}
          </div>
        </>
      )}

      {done && (
        <button
          onClick={() => navigate("/apps")}
          style={{
            marginTop: "1rem",
            padding: "0.5rem 1rem",
            background: "#f5f5f5",
            border: "1px solid #e5e7eb",
            borderRadius: 6,
            cursor: "pointer",
            fontSize: "0.85rem",
            fontFamily: "monospace",
          }}
        >
          ← Back to Apps
        </button>
      )}

      {error && (
        <div
          style={{ marginTop: "0.5rem", color: "#ef4444", fontSize: "0.82rem" }}
        >
          {error}
        </div>
      )}
    </div>
  );
}

// ─── Installed app detail page ────────────────────────────────────────────────

function InstalledDetailPage({
  instanceName,
  navigate,
}: {
  instanceName: string;
  navigate: (to: string) => void;
}) {
  const [app, setApp] = useState<InstalledApp | null>(null);
  const [pods, setPods] = useState<Pod[]>([]);
  const [selectedPod, setSelectedPod] = useState<string | null>(null);
  const [view, setView] = useState<"describe" | "logs">("describe");
  const [describe, setDescribe] = useState("");
  const [logs, setLogs] = useState<string[]>([]);
  const [scanning, setScanning] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [uninstalling, setUninstalling] = useState(false);
  const [uninstallError, setUninstallError] = useState("");
  const outputRef = useRef<HTMLDivElement>(null);
  const readerRef = useRef<ReadableStreamDefaultReader | null>(null);

  function loadApp() {
    fetch("/api/apps")
      .then((r) => r.json())
      .then((apps: InstalledApp[]) => {
        const found = apps.find((a) => a.instance_name === instanceName);
        if (found) setApp(found);
      })
      .catch(() => {});
  }

  function loadPods() {
    fetch(`/api/apps/${instanceName}/pods`)
      .then((r) => r.json())
      .then(setPods)
      .catch(() => {});
  }

  useEffect(() => {
    loadApp();
    loadPods();
    const id = setInterval(() => {
      loadApp();
      loadPods();
    }, 3000);
    return () => clearInterval(id);
  }, [instanceName]);

  useEffect(() => {
    if (outputRef.current)
      outputRef.current.scrollTop = outputRef.current.scrollHeight;
  }, [logs, describe]);

  async function selectPod(podName: string) {
    if (readerRef.current) {
      readerRef.current.cancel();
      readerRef.current = null;
    }
    setSelectedPod(podName);
    setView("describe");
    setDescribe("");
    setLogs([]);
    const r = await fetch(
      `/api/apps/${instanceName}/describe/${podName}`,
    ).catch(() => null);
    if (r?.ok) setDescribe((await r.json()).output);
  }

  async function showLogs(podName: string) {
    if (readerRef.current) {
      readerRef.current.cancel();
      readerRef.current = null;
    }
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

  useEffect(
    () => () => {
      readerRef.current?.cancel();
    },
    [],
  );

  async function scanOutputs() {
    setScanning(true);
    const r = await fetch(`/api/apps/${instanceName}/scan-outputs`, {
      method: "POST",
    }).catch(() => null);
    if (r?.ok) {
      const data = await r.json();
      setApp((prev) => (prev ? { ...prev, outputs: data.outputs } : prev));
    }
    setScanning(false);
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
      const d = await r.json().catch(() => ({}));
      setUninstallError(d.detail ?? "Uninstall failed");
      setUninstalling(false);
      setConfirming(false);
    }
  }

if (!app)
    return (
      <div>
        <BackButton onClick={() => navigate("/apps")} />
        <div style={{ color: "#999", fontSize: "0.85rem" }}>Loading…</div>
      </div>
    );

  const visibleOutputs = app.outputs.filter((o) => o.type !== "hidden");
  const configEntries = Object.entries(app.config).filter(
    ([, v]) => v !== null && v !== undefined && v !== "",
  );

  return (
    <div>
      <BackButton onClick={() => navigate("/apps")} />

      {/* Header */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          marginBottom: "0.25rem",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "0.6rem" }}>
          <h2 style={{ margin: 0, fontSize: "1.2rem" }}>{app.app_id}</h2>
          <StatusBadge status={app.status} />
        </div>
        <div style={{ display: "flex", gap: "0.5rem", alignItems: "center" }}>
<button
            onClick={doUninstall}
            disabled={uninstalling || app.status === "uninstalling"}
            style={{
              fontSize: "0.75rem",
              padding: "0.3rem 0.7rem",
              borderRadius: 5,
              cursor: "pointer",
              border: `1px solid ${confirming ? "#ef4444" : "#fca5a5"}`,
              background: confirming ? "#ef4444" : "#fff",
              color: confirming ? "#fff" : "#ef4444",
            }}
          >
            {uninstalling ? "Removing…" : confirming ? "Confirm" : "Uninstall"}
          </button>
        </div>
      </div>
      <div
        style={{ fontSize: "0.78rem", color: "#888", marginBottom: "0.5rem" }}
      >
        {instanceName}
      </div>
      {uninstallError && (
        <div
          style={{
            color: "#ef4444",
            fontSize: "0.75rem",
            marginBottom: "0.5rem",
          }}
        >
          {uninstallError}
        </div>
      )}

      {/* Inputs */}
      {configEntries.length > 0 && (
        <>
          <SectionHeader title="Inputs" />
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "auto 1fr",
              gap: "0.3rem 1rem",
              fontSize: "0.83rem",
            }}
          >
            {configEntries.map(([k, v]) => (
              <>
                <span
                  key={`k-${k}`}
                  style={{ color: "#888", whiteSpace: "nowrap" }}
                >
                  {k}
                </span>
                <span
                  key={`v-${k}`}
                  style={{ fontFamily: "monospace", wordBreak: "break-all" }}
                >
                  {formatConfigValue(v)}
                </span>
              </>
            ))}
          </div>
        </>
      )}

      {/* Outputs */}
      <SectionHeader title="Outputs" />
      <div
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "0.4rem",
          marginBottom: "0.75rem",
        }}
      >
        {visibleOutputs.length === 0 && (
          <div style={{ fontSize: "0.82rem", color: "#999" }}>
            No outputs scanned yet.
          </div>
        )}
        {visibleOutputs.map((o) => (
          <div
            key={o.key}
            style={{ display: "flex", alignItems: "center", gap: "0.75rem" }}
          >
            <span style={{ fontSize: "0.72rem", color: "#888", minWidth: 80 }}>
              {o.label || o.key}
            </span>
            {o.type === "url" ? (
              <a
                href={o.value}
                target="_blank"
                rel="noreferrer"
                style={{
                  fontSize: "0.83rem",
                  color: "#7c3aed",
                  fontFamily: "monospace",
                }}
              >
                {o.value}
              </a>
            ) : (
              <span style={{ fontSize: "0.83rem", fontFamily: "monospace" }}>
                {o.value}
              </span>
            )}
            <button
              onClick={() => copyText(o.value)}
              style={{
                background: "none",
                border: "none",
                cursor: "pointer",
                fontSize: "0.65rem",
                color: "#bbb",
                padding: 0,
              }}
            >
              copy
            </button>
          </div>
        ))}
      </div>
      <button
        onClick={scanOutputs}
        disabled={scanning}
        style={{
          fontSize: "0.78rem",
          padding: "0.3rem 0.75rem",
          border: "1px solid #e5e7eb",
          borderRadius: 5,
          cursor: scanning ? "not-allowed" : "pointer",
          background: "#fff",
          color: scanning ? "#999" : "#333",
          fontFamily: "monospace",
        }}
      >
        {scanning ? "Scanning…" : "Scan outputs"}
      </button>

      {/* Pods */}
      <SectionHeader title="Pods" />
      <div
        style={{
          display: "flex",
          flexWrap: "wrap",
          gap: "0.4rem",
          marginBottom: "0.75rem",
        }}
      >
        {pods.map((pod) => (
          <button
            key={pod.name}
            onClick={() => selectPod(pod.name)}
            style={{
              display: "flex",
              alignItems: "center",
              gap: "0.4rem",
              padding: "0.3rem 0.7rem",
              borderRadius: 6,
              cursor: "pointer",
              fontSize: "0.78rem",
              border:
                selectedPod === pod.name
                  ? "2px solid #1a1a1a"
                  : "1px solid #e5e7eb",
              background: selectedPod === pod.name ? "#f5f5f5" : "#fff",
            }}
          >
            <span
              style={{
                width: 8,
                height: 8,
                borderRadius: "50%",
                background: phaseColor(pod),
                display: "inline-block",
                flexShrink: 0,
              }}
            />
            {pod.name}
          </button>
        ))}
        {pods.length === 0 && (
          <div style={{ fontSize: "0.8rem", color: "#999" }}>
            No pods found…
          </div>
        )}
      </div>

      {selectedPod && (
        <>
          <div
            style={{ display: "flex", gap: "0.25rem", marginBottom: "0.5rem" }}
          >
            {(["describe", "logs"] as const).map((v) => (
              <button
                key={v}
                onClick={() =>
                  v === "logs" ? showLogs(selectedPod) : setView("describe")
                }
                style={{
                  padding: "0.25rem 0.75rem",
                  fontSize: "0.78rem",
                  borderRadius: 4,
                  cursor: "pointer",
                  background: view === v ? "#1a1a1a" : "#f5f5f5",
                  color: view === v ? "#fff" : "#333",
                  border: "none",
                }}
              >
                {v}
              </button>
            ))}
          </div>
          <div
            ref={outputRef}
            style={{
              background: "#111",
              color: "#d1fae5",
              borderRadius: 6,
              padding: "0.75rem",
              fontSize: "0.72rem",
              overflowY: "auto",
              fontFamily: "monospace",
              whiteSpace: "pre-wrap",
              minHeight: 200,
              maxHeight: 400,
            }}
          >
            {view === "describe" ? (
              describe || <span style={{ color: "#666" }}>Loading…</span>
            ) : logs.length === 0 ? (
              <span style={{ color: "#666" }}>Waiting for logs…</span>
            ) : (
              logs.map((l, i) => <div key={i}>{l}</div>)
            )}
          </div>
        </>
      )}
    </div>
  );
}

// ─── Router ───────────────────────────────────────────────────────────────────

export function AppsPage({
  path,
  navigate,
}: {
  path: string;
  navigate: (to: string) => void;
}) {
  if (path.startsWith("/apps/")) {
    const appId = path.slice("/apps/".length);
    if (appId) return <AppInstallPage appId={appId} navigate={navigate} />;
  }
  if (path.startsWith("/installed/")) {
    const instanceName = path.slice("/installed/".length);
    if (instanceName)
      return (
        <InstalledDetailPage instanceName={instanceName} navigate={navigate} />
      );
  }
  return <MainAppsPage navigate={navigate} />;
}

import { useEffect, useState } from "react";
import Form from "@rjsf/core";
import type { IChangeEvent } from "@rjsf/core";
import type { RJSFSchema, UiSchema, WidgetProps } from "@rjsf/utils";
import validator from "@rjsf/validator-ajv8";

function PasswordWidget({ id, value, onChange, options }: WidgetProps) {
  const generate = () => {
    const chars = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    const bytes = new Uint8Array(20);
    crypto.getRandomValues(bytes);
    onChange(Array.from(bytes).map((b) => chars[b % chars.length]).join(""));
  };

  useEffect(() => {
    if (!value && options.generateButton) generate();
  }, []);

  return (
    <div style={{ display: "flex", gap: "0.5rem", alignItems: "center" }}>
      <input
        id={id}
        type="password"
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value)}
        style={{
          flex: 1,
          padding: "0.4rem 0.6rem",
          background: "#111",
          color: "#eee",
          border: "1px solid #444",
          borderRadius: 4,
          fontFamily: "monospace",
          fontSize: "0.9rem",
        }}
      />
      {options.generateButton && (
        <button type="button" onClick={generate} style={{ padding: "0.35rem 0.75rem", fontSize: "0.8rem", whiteSpace: "nowrap" }}>
          Generate
        </button>
      )}
    </div>
  );
}

const widgets = { password: PasswordWidget };

// ── Types ─────────────────────────────────────────────────────────────────────

interface AppMeta {
  id: string;
  name: string;
  description: string;
  icon: string;
  category: string;
  volumes?: Array<{ name: string; description: string }>;
}

interface InstalledApp {
  app_id: string;
  namespace: string;
  tunnel_url: string;
}

interface Disk {
  disk_id: string;
  label: string | null;
  mount_path: string;
  status: string;
  node_hostname: string;
  node_wg_ipv6: string;
  total_bytes: number | null;
  free_bytes: number | null;
}

interface DiskSpec {
  disk_id: string;
  node_wg_ipv6: string;
  mount_path: string;
}

type AppStatus = "running" | "starting" | "error" | "not_installed";

// ── Disk picker ───────────────────────────────────────────────────────────────

function DiskPicker({
  volume,
  disks,
  selected,
  onChange,
}: {
  volume: { name: string; description: string };
  disks: Disk[];
  selected: DiskSpec[];
  onChange: (specs: DiskSpec[]) => void;
}) {
  const usable = disks.filter((d) => d.status === "registered" && d.mount_path);

  const toggle = (disk: Disk) => {
    const already = selected.some((s) => s.disk_id === disk.disk_id);
    if (already) {
      onChange(selected.filter((s) => s.disk_id !== disk.disk_id));
    } else {
      onChange([
        ...selected,
        { disk_id: disk.disk_id, node_wg_ipv6: disk.node_wg_ipv6 ?? "", mount_path: disk.mount_path },
      ]);
    }
  };

  const fmt = (bytes: number | null) => {
    if (!bytes) return "";
    const gb = bytes / 1e9;
    return gb >= 1000 ? `${(gb / 1000).toFixed(1)} TB` : `${gb.toFixed(0)} GB`;
  };

  return (
    <div style={{ marginBottom: "1rem" }}>
      <div style={{ fontSize: "0.85rem", fontWeight: "bold", marginBottom: "0.25rem" }}>
        {volume.name}
      </div>
      <div style={{ fontSize: "0.78rem", color: "#888", marginBottom: "0.5rem" }}>
        {volume.description}
      </div>
      {usable.length === 0 ? (
        <div style={{ fontSize: "0.82rem", color: "#f87171" }}>
          No initialized disks available. Initialize a disk first from the Disks tab.
        </div>
      ) : (
        <div style={{ display: "flex", flexDirection: "column", gap: "0.35rem" }}>
          {usable.map((disk) => {
            const isSelected = selected.some((s) => s.disk_id === disk.disk_id);
            return (
              <label
                key={disk.disk_id}
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "0.6rem",
                  padding: "0.45rem 0.6rem",
                  border: `1px solid ${isSelected ? "#86efac" : "#333"}`,
                  borderRadius: 5,
                  cursor: "pointer",
                  background: isSelected ? "#0f2a1a" : "#111",
                  fontSize: "0.82rem",
                }}
              >
                <input
                  type="checkbox"
                  checked={isSelected}
                  onChange={() => toggle(disk)}
                  style={{ accentColor: "#86efac" }}
                />
                <span style={{ flex: 1, fontFamily: "monospace" }}>
                  {disk.label || disk.disk_id}
                </span>
                <span style={{ color: "#888" }}>{disk.node_hostname}</span>
                {disk.total_bytes && (
                  <span style={{ color: "#666" }}>
                    {fmt(disk.free_bytes)} free / {fmt(disk.total_bytes)}
                  </span>
                )}
              </label>
            );
          })}
        </div>
      )}
    </div>
  );
}

// ── Install modal ─────────────────────────────────────────────────────────────

function InstallModal({ app, onClose, onDone }: { app: AppMeta; onClose: () => void; onDone: () => void }) {
  const [schema, setSchema] = useState<RJSFSchema | null>(null);
  const [uiSchema, setUiSchema] = useState<UiSchema>({});
  const [disks, setDisks] = useState<Disk[]>([]);
  const [volumeSelections, setVolumeSelections] = useState<Record<string, DiskSpec[]>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    Promise.all([
      fetch(`/api/apps/${app.id}/schema`).then((r) => r.json()),
      fetch(`/api/apps/${app.id}/uischema`).then((r) => r.json()),
      fetch("/api/disks").then((r) => r.json()).catch(() => []),
    ]).then(([s, u, d]) => {
      setSchema(s);
      setUiSchema(u);
      setDisks(Array.isArray(d) ? d : []);
      const init: Record<string, DiskSpec[]> = {};
      for (const vol of app.volumes ?? []) init[vol.name] = [];
      setVolumeSelections(init);
    });
  }, [app.id]);

  const hasVolumes = (app.volumes ?? []).length > 0;

  const volumesComplete = !hasVolumes || (app.volumes ?? []).every(
    (v) => (volumeSelections[v.name] ?? []).length > 0
  );

  const handleSubmit = async ({ formData }: IChangeEvent) => {
    if (!volumesComplete) {
      setError("Select at least one disk for each volume.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const body = hasVolumes
        ? { ...formData, volumes: volumeSelections }
        : formData;
      const res = await fetch(`/api/apps/${app.id}/install`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        setError(body.detail ?? "Installation failed");
      } else {
        onDone();
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div style={{
      position: "fixed", inset: 0,
      background: "rgba(0,0,0,0.75)",
      display: "flex", alignItems: "center", justifyContent: "center",
      zIndex: 200,
    }}>
      <div style={{
        background: "#1a1a1a",
        border: "1px solid #333",
        borderRadius: 10,
        padding: "1.5rem",
        width: "min(540px, 92vw)",
        maxHeight: "88vh",
        overflowY: "auto",
      }}>
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: "1.25rem" }}>
          <span style={{ fontWeight: "bold", fontSize: "1rem" }}>
            {app.icon}&nbsp; Install {app.name}
          </span>
          <button onClick={onClose} style={{ background: "none", border: "none", color: "#666", fontSize: "1.1rem", cursor: "pointer", padding: "0 0.25rem" }}>✕</button>
        </div>

        {!schema ? (
          <div style={{ color: "#666", fontSize: "0.9rem" }}>Loading…</div>
        ) : (
          <>
            {hasVolumes && (
              <div style={{ marginBottom: "1rem", borderBottom: "1px solid #2a2a2a", paddingBottom: "1rem" }}>
                <div style={{ fontSize: "0.82rem", color: "#888", marginBottom: "0.75rem", textTransform: "uppercase", letterSpacing: "0.05em" }}>
                  Storage
                </div>
                {(app.volumes ?? []).map((vol) => (
                  <DiskPicker
                    key={vol.name}
                    volume={vol}
                    disks={disks}
                    selected={volumeSelections[vol.name] ?? []}
                    onChange={(specs) =>
                      setVolumeSelections((prev) => ({ ...prev, [vol.name]: specs }))
                    }
                  />
                ))}
              </div>
            )}

            <Form
              className="yolab-form"
              schema={schema}
              uiSchema={uiSchema}
              validator={validator}
              widgets={widgets}
              onSubmit={handleSubmit}
              disabled={busy}
            >
              {error && (
                <div style={{ color: "#f87171", fontSize: "0.85rem", marginBottom: "0.75rem" }}>
                  {error}
                </div>
              )}
              <button
                type="submit"
                disabled={busy || !volumesComplete}
                style={{ width: "100%", marginTop: "0.5rem", opacity: !volumesComplete ? 0.5 : 1 }}
              >
                {busy ? "Installing…" : `Install ${app.name}`}
              </button>
            </Form>
          </>
        )}
      </div>
    </div>
  );
}

// ── Installed app row ─────────────────────────────────────────────────────────

function InstalledRow({ app_id, tunnel_url, onRemove }: { app_id: string; tunnel_url: string; onRemove: () => void }) {
  const [status, setStatus] = useState<AppStatus>("starting");
  const [confirmWipe, setConfirmWipe] = useState(false);

  useEffect(() => {
    const poll = () =>
      fetch(`/api/apps/${app_id}/status`)
        .then((r) => r.json())
        .then((d) => setStatus(d.status))
        .catch(() => { });
    poll();
    const id = setInterval(poll, 6000);
    return () => clearInterval(id);
  }, [app_id]);

  const remove = async (wipe: boolean) => {
    await fetch(`/api/apps/${app_id}?wipe=${wipe}`, { method: "DELETE" });
    setConfirmWipe(false);
    onRemove();
  };

  const dotColor = status === "running" ? "#86efac" : status === "error" ? "#f87171" : "#facc15";
  const dotLabel = status === "running" ? "Running" : status === "error" ? "Error" : "Starting…";

  return (
    <div style={{ display: "flex", alignItems: "center", gap: "1rem", padding: "0.65rem 0", borderBottom: "1px solid #2a2a2a" }}>
      <span style={{ width: 8, height: 8, borderRadius: "50%", background: dotColor, display: "inline-block", flexShrink: 0 }} />
      <span style={{ flex: 1, fontFamily: "monospace" }}>{app_id}</span>
      <span style={{ color: dotColor, fontSize: "0.8rem", width: 70 }}>{dotLabel}</span>
      {tunnel_url && (
        <button
          onClick={() => window.open(tunnel_url, "_blank")}
          style={{ padding: "0.3rem 0.75rem", fontSize: "0.8rem" }}
        >
          Open ↗
        </button>
      )}
      {confirmWipe ? (
        <>
          <button onClick={() => remove(false)} style={{ padding: "0.3rem 0.75rem", fontSize: "0.8rem" }}>Keep data</button>
          <button onClick={() => remove(true)} style={{ padding: "0.3rem 0.75rem", fontSize: "0.8rem", color: "#f87171" }}>Wipe data</button>
          <button onClick={() => setConfirmWipe(false)} style={{ padding: "0.3rem 0.5rem", fontSize: "0.8rem", background: "none", border: "none", color: "#666", cursor: "pointer" }}>✕</button>
        </>
      ) : (
        <button onClick={() => setConfirmWipe(true)} style={{ padding: "0.3rem 0.75rem", fontSize: "0.8rem", color: "#f87171" }}>
          Remove
        </button>
      )}
    </div>
  );
}

// ── App card ──────────────────────────────────────────────────────────────────

function AppCard({ app, onInstall }: { app: AppMeta; onInstall: () => void }) {
  return (
    <div style={{
      border: "1px solid #2a2a2a",
      borderRadius: 8,
      padding: "1rem",
      display: "flex",
      flexDirection: "column",
      gap: "0.4rem",
      transition: "border-color 0.2s",
    }}
      onMouseEnter={(e) => (e.currentTarget.style.borderColor = "#555")}
      onMouseLeave={(e) => (e.currentTarget.style.borderColor = "#2a2a2a")}
    >
      <div style={{ fontSize: "1.75rem", lineHeight: 1 }}>{app.icon}</div>
      <div style={{ fontWeight: "bold", fontSize: "0.95rem" }}>{app.name}</div>
      <div style={{ color: "#777", fontSize: "0.82rem", flex: 1, lineHeight: 1.4 }}>{app.description}</div>
      <button onClick={onInstall} style={{ marginTop: "0.5rem", fontSize: "0.85rem", padding: "0.4rem 0" }}>
        Install
      </button>
    </div>
  );
}

// ── Services page (installed) ──────────────────────────────────────────────────

export function ServicesPage() {
  const [installed, setInstalled] = useState<InstalledApp[]>([]);
  const [loading, setLoading] = useState(true);

  const refresh = () => {
    fetch("/api/apps/installed")
      .then((r) => r.json())
      .then((d) => { setInstalled(Array.isArray(d) ? d : []); setLoading(false); })
      .catch(() => setLoading(false));
  };

  useEffect(() => { refresh(); }, []);

  return (
    <div>
      <h2 style={{ fontSize: "1.2rem", marginBottom: "1rem" }}>Services</h2>
      {loading ? (
        <div style={{ color: "#666" }}>Loading…</div>
      ) : installed.length === 0 ? (
        <div style={{ color: "#555", fontSize: "0.9rem" }}>
          No services running. Install one from the <strong>Services Store</strong>.
        </div>
      ) : (
        installed.map((i) => (
          <InstalledRow key={i.app_id} app_id={i.app_id} tunnel_url={i.tunnel_url} onRemove={refresh} />
        ))
      )}
    </div>
  );
}

// ── Services Store page (catalog) ──────────────────────────────────────────────

export function ServicesStorePage() {
  const [catalog, setCatalog] = useState<AppMeta[]>([]);
  const [installed, setInstalled] = useState<InstalledApp[]>([]);
  const [modal, setModal] = useState<AppMeta | null>(null);

  const refresh = () => {
    fetch("/api/apps").then((r) => r.json()).then((d) => setCatalog(Array.isArray(d) ? d : [])).catch(() => { });
    fetch("/api/apps/installed").then((r) => r.json()).then((d) => setInstalled(Array.isArray(d) ? d : [])).catch(() => { });
  };

  useEffect(() => { refresh(); }, []);

  const installedIds = new Set(installed.map((i) => i.app_id));
  const available = catalog.filter((a) => !installedIds.has(a.id));

  return (
    <div>
      <h2 style={{ fontSize: "1.2rem", marginBottom: "1rem" }}>Services Store</h2>
      {available.length === 0 ? (
        <div style={{ color: "#555", fontSize: "0.9rem" }}>No services available in catalog.</div>
      ) : (
        <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fill, minmax(190px, 1fr))", gap: "0.75rem" }}>
          {available.map((app) => (
            <AppCard key={app.id} app={app} onInstall={() => setModal(app)} />
          ))}
        </div>
      )}

      {modal && (
        <InstallModal
          app={modal}
          onClose={() => setModal(null)}
          onDone={() => { setModal(null); refresh(); }}
        />
      )}
    </div>
  );
}

import { useEffect, useState } from "react";
import { SseModal } from "../components/SseModal";

interface Disk {
  disk_id: string;
  device: string;
  label: string | null;
  type: string;
  mount_path: string | null;
  status: "system" | "unformatted" | "registered" | "incompatible" | "unconfigured_network";
  service_name: string | null;
  total_bytes: number | null;
  free_bytes: number | null;
  data_written: boolean;
  node_hostname?: string;
}

// Deterministic color per service name — same service = same color across all disks/nodes.
const SERVICE_PALETTE = [
  "#f87171", // red
  "#fb923c", // orange
  "#fbbf24", // amber
  "#a3e635", // lime
  "#34d399", // emerald
  "#38bdf8", // sky
  "#818cf8", // indigo
  "#e879f9", // fuchsia
  "#f472b6", // pink
  "#2dd4bf", // teal
];

function serviceColor(name: string): string {
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) & 0xffff;
  return SERVICE_PALETTE[h % SERVICE_PALETTE.length];
}

function fmtBytes(b: number | null) {
  if (b === null) return "—";
  if (b >= 1e12) return (b / 1e12).toFixed(1) + " TB";
  if (b >= 1e9) return (b / 1e9).toFixed(1) + " GB";
  return (b / 1e6).toFixed(0) + " MB";
}

function UsageBar({ disk }: { disk: Disk }) {
  const { total_bytes, free_bytes, status, service_name } = disk;
  if (!total_bytes || free_bytes === null) return <span style={{ color: "#444" }}>—</span>;

  const used = total_bytes - free_bytes;
  const usedPct = Math.min(100, Math.round((used / total_bytes) * 100));
  const freePct = 100 - usedPct;

  let usedColor: string;
  if (status === "system") usedColor = "#64748b";           // slate grey
  else if (service_name) usedColor = serviceColor(service_name);
  else usedColor = "#fbbf24";                               // amber = registered but unassigned

  return (
    <div style={{ display: "flex", alignItems: "center", gap: "0.6rem", minWidth: 200 }}>
      <div style={{ flex: 1, height: 8, background: "#1e1e1e", borderRadius: 4, overflow: "hidden", display: "flex" }}>
        <div style={{ width: `${usedPct}%`, height: "100%", background: usedColor, borderRadius: "4px 0 0 4px", flexShrink: 0 }} />
        <div style={{ width: `${freePct}%`, height: "100%", background: "#2a2a2a", flexShrink: 0 }} />
      </div>
      <span style={{ fontSize: "0.78rem", color: "#888", whiteSpace: "nowrap" }}>
        {fmtBytes(free_bytes)} free / {fmtBytes(total_bytes)}
      </span>
    </div>
  );
}

// Legend showing which color maps to which service (derived from visible disks).
function ServiceLegend({ disks }: { disks: Disk[] }) {
  const services = Array.from(
    new Set(disks.map((d) => d.service_name).filter((s): s is string => !!s))
  );
  if (services.length === 0) return null;
  return (
    <div style={{ display: "flex", gap: "1rem", flexWrap: "wrap", marginBottom: "1rem", fontSize: "0.8rem" }}>
      {services.map((s) => (
        <span key={s} style={{ display: "flex", alignItems: "center", gap: "0.35rem" }}>
          <span style={{ width: 10, height: 10, borderRadius: 2, background: serviceColor(s), display: "inline-block" }} />
          <span style={{ color: "#bbb" }}>{s}</span>
        </span>
      ))}
    </div>
  );
}

export function DisksPage() {
  const [disks, setDisks] = useState<Disk[]>([]);
  const [loading, setLoading] = useState(true);
  const [modal, setModal] = useState<{ url: string; title: string } | null>(null);
  const [confirmWipe, setConfirmWipe] = useState<string | null>(null);

  function reload() {
    fetch("/api/disks")
      .then((r) => r.json())
      .then((data) => { setDisks(Array.isArray(data) ? data : []); setLoading(false); })
      .catch(() => setLoading(false));
  }

  useEffect(() => { reload(); }, []);

  function handleModalClose() {
    setModal(null);
    reload();
  }

  // Group by node hostname when present.
  const byNode = disks.reduce<Record<string, Disk[]>>((acc, d) => {
    const key = d.node_hostname ?? "This node";
    (acc[key] ??= []).push(d);
    return acc;
  }, {});

  return (
    <div>
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline", marginBottom: "1rem" }}>
        <h2 style={{ fontSize: "1.2rem", margin: 0 }}>Disks</h2>
      </div>

      <ServiceLegend disks={disks} />

      {loading ? (
        <div style={{ color: "#666" }}>Loading…</div>
      ) : disks.length === 0 ? (
        <div style={{ color: "#666" }}>No disks found.</div>
      ) : (
        Object.entries(byNode).map(([hostname, nodeDisks]) => (
          <div key={hostname} style={{ marginBottom: "2rem" }}>
            {Object.keys(byNode).length > 1 && (
              <div style={{ fontSize: "0.75rem", color: "#555", textTransform: "uppercase", letterSpacing: "0.08em", marginBottom: "0.5rem" }}>
                {hostname}
              </div>
            )}
            <table style={{ width: "100%", borderCollapse: "collapse", fontSize: "0.9rem" }}>
              <thead>
                <tr style={{ borderBottom: "1px solid #2a2a2a" }}>
                  {["Device", "Label", "Service", "Usage", ""].map((h) => (
                    <th key={h} style={{ textAlign: "left", padding: "0.4rem 0.6rem", color: "#555", fontWeight: "normal", fontSize: "0.78rem" }}>{h}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {nodeDisks.map((d) => (
                  <tr key={d.disk_id} style={{ borderBottom: "1px solid #1a1a1a" }}>
                    <td style={{ padding: "0.55rem 0.6rem", fontFamily: "monospace", fontSize: "0.8rem", color: "#aaa" }}>{d.device}</td>
                    <td style={{ padding: "0.55rem 0.6rem", color: "#888" }}>{d.label ?? "—"}</td>
                    <td style={{ padding: "0.55rem 0.6rem" }}>
                      {d.service_name ? (
                        <span style={{ display: "inline-flex", alignItems: "center", gap: "0.35rem" }}>
                          <span style={{ width: 8, height: 8, borderRadius: 2, background: serviceColor(d.service_name), display: "inline-block" }} />
                          <span style={{ color: "#ccc", fontSize: "0.85rem" }}>{d.service_name}</span>
                        </span>
                      ) : d.status === "system" ? (
                        <span style={{ color: "#555", fontSize: "0.85rem" }}>system</span>
                      ) : (
                        <span style={{ color: "#444", fontSize: "0.85rem" }}>—</span>
                      )}
                    </td>
                    <td style={{ padding: "0.55rem 0.6rem", minWidth: 220 }}>
                      <UsageBar disk={d} />
                    </td>
                    <td style={{ padding: "0.55rem 0.6rem" }}>
                      {d.status === "unformatted" && (
                        <button
                          onClick={() => setModal({ url: `/api/node-agent/disks/${d.disk_id}/init`, title: `Initialize ${d.device}` })}
                          style={{ padding: "0.25rem 0.7rem", background: "#1a1a1a", color: "#fbbf24", border: "1px solid #fbbf24", borderRadius: 4, cursor: "pointer", fontSize: "0.8rem" }}
                        >
                          Initialize
                        </button>
                      )}
                      {d.status === "incompatible" && (
                        confirmWipe === d.disk_id ? (
                          <span style={{ fontSize: "0.8rem" }}>
                            <span style={{ color: "#f87171" }}>Erase all data? </span>
                            <button onClick={() => { setConfirmWipe(null); setModal({ url: `/api/node-agent/disks/${d.disk_id}/wipe-init`, title: `Wipe & Initialize ${d.device}` }); }}
                              style={{ marginRight: "0.4rem", padding: "0.2rem 0.5rem", background: "#7f1d1d", color: "#fca5a5", border: "none", borderRadius: 3, cursor: "pointer" }}>Yes</button>
                            <button onClick={() => setConfirmWipe(null)}
                              style={{ padding: "0.2rem 0.5rem", background: "#222", color: "#aaa", border: "none", borderRadius: 3, cursor: "pointer" }}>No</button>
                          </span>
                        ) : (
                          <button onClick={() => setConfirmWipe(d.disk_id)}
                            style={{ padding: "0.25rem 0.7rem", background: "#1a1a1a", color: "#f87171", border: "1px solid #f87171", borderRadius: 4, cursor: "pointer", fontSize: "0.8rem" }}>
                            Wipe &amp; Initialize
                          </button>
                        )
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ))
      )}

      {modal && <SseModal url={modal.url} onClose={handleModalClose} />}
    </div>
  );
}

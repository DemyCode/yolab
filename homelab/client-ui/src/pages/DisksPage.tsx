import { useEffect, useState } from "react";
import { SseModal } from "../components/SseModal";

interface Disk {
  disk_id: string;
  device: string;
  label: string | null;
  type: string;
  mount_path: string | null;
  status: "unformatted" | "registered" | "incompatible" | "unconfigured_network";
  total_bytes: number | null;
  free_bytes: number | null;
  data_written: boolean;
}

const STATUS_COLOR: Record<string, string> = {
  registered: "#86efac",
  unformatted: "#fbbf24",
  incompatible: "#f87171",
  unconfigured_network: "#f87171",
};

function fmtBytes(b: number | null) {
  if (b === null) return "—";
  if (b >= 1e12) return (b / 1e12).toFixed(1) + " TB";
  if (b >= 1e9) return (b / 1e9).toFixed(1) + " GB";
  return (b / 1e6).toFixed(0) + " MB";
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

  return (
    <div>
      <h2 style={{ fontSize: "1.2rem", marginBottom: "1rem" }}>Disks</h2>

      {loading ? (
        <div style={{ color: "#666" }}>Loading…</div>
      ) : disks.length === 0 ? (
        <div style={{ color: "#666" }}>No disks found.</div>
      ) : (
        <table style={{ width: "100%", borderCollapse: "collapse", fontSize: "0.9rem" }}>
          <thead>
            <tr style={{ borderBottom: "1px solid #333" }}>
              {["Device", "Label", "Type", "Status", "Free / Total", ""].map((h) => (
                <th key={h} style={{ textAlign: "left", padding: "0.4rem 0.6rem", color: "#999" }}>{h}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {disks.map((d) => {
              const usedPct = d.total_bytes && d.free_bytes !== null
                ? Math.round((1 - d.free_bytes / d.total_bytes) * 100)
                : null;
              return (
                <tr key={d.disk_id} style={{ borderBottom: "1px solid #222" }}>
                  <td style={{ padding: "0.4rem 0.6rem", fontFamily: "monospace", fontSize: "0.8rem" }}>{d.device}</td>
                  <td style={{ padding: "0.4rem 0.6rem", color: "#aaa" }}>{d.label ?? "—"}</td>
                  <td style={{ padding: "0.4rem 0.6rem", color: "#aaa" }}>{d.type}</td>
                  <td style={{ padding: "0.4rem 0.6rem" }}>
                    <span style={{ color: STATUS_COLOR[d.status] ?? "#eee" }}>{d.status}</span>
                  </td>
                  <td style={{ padding: "0.4rem 0.6rem", color: "#aaa" }}>
                    {d.status === "registered" && usedPct !== null ? (
                      <div style={{ display: "flex", alignItems: "center", gap: "0.5rem" }}>
                        <div style={{ width: 80, height: 6, background: "#333", borderRadius: 3 }}>
                          <div style={{ width: `${usedPct}%`, height: "100%", background: usedPct > 90 ? "#f87171" : "#86efac", borderRadius: 3 }} />
                        </div>
                        <span style={{ fontSize: "0.8rem" }}>{fmtBytes(d.free_bytes)} / {fmtBytes(d.total_bytes)}</span>
                      </div>
                    ) : "—"}
                  </td>
                  <td style={{ padding: "0.4rem 0.6rem" }}>
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
              );
            })}
          </tbody>
        </table>
      )}

      {modal && <SseModal url={modal.url} onClose={handleModalClose} />}
    </div>
  );
}

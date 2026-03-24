import { useEffect, useState } from "react";
import { SseModal } from "../components/SseModal";

interface Volume {
  service_name: string;
  volume_name: string;
  disk_paths: string[];
  mergerfs_path: string | null;
  status: string;
}

export function VolumesPage() {
  const [volumes, setVolumes] = useState<Volume[]>([]);
  const [loading, setLoading] = useState(true);
  const [reorg, setReorg] = useState<{ vol: Volume; newPaths: string[] } | null>(null);
  const [estimate, setEstimate] = useState<number | null>(null);
  const [confirmed, setConfirmed] = useState(false);

  function reload() {
    fetch("/api/volumes")
      .then((r) => r.json())
      .then((data) => { setVolumes(Array.isArray(data) ? data : []); setLoading(false); })
      .catch(() => setLoading(false));
  }

  useEffect(() => { reload(); }, []);

  function openReorg(vol: Volume) {
    setReorg({ vol, newPaths: [...vol.disk_paths] });
    setEstimate(null);
    setConfirmed(false);
  }

  function moveUp(idx: number) {
    if (!reorg || idx === 0) return;
    const p = [...reorg.newPaths];
    [p[idx - 1], p[idx]] = [p[idx], p[idx - 1]];
    setReorg({ ...reorg, newPaths: p });
    setEstimate(null);
    setConfirmed(false);
  }

  function moveDown(idx: number) {
    if (!reorg || idx === reorg.newPaths.length - 1) return;
    const p = [...reorg.newPaths];
    [p[idx], p[idx + 1]] = [p[idx + 1], p[idx]];
    setReorg({ ...reorg, newPaths: p });
    setEstimate(null);
    setConfirmed(false);
  }

  async function fetchEstimate() {
    if (!reorg) return;
    const { vol, newPaths } = reorg;
    const qs = encodeURIComponent(newPaths.join(","));
    const resp = await fetch(
      `/api/node-agent/volumes/${vol.service_name}/${vol.volume_name}/reorganize-estimate?new_disk_paths=${qs}`
    );
    const data = await resp.json();
    setEstimate(data.bytes_to_move ?? 0);
  }

  function fmtBytes(b: number) {
    if (b >= 1e12) return (b / 1e12).toFixed(1) + " TB";
    if (b >= 1e9) return (b / 1e9).toFixed(1) + " GB";
    return (b / 1e6).toFixed(0) + " MB";
  }

  return (
    <div>
      <h2 style={{ fontSize: "1.2rem", marginBottom: "1rem" }}>Volumes</h2>

      {loading ? (
        <div style={{ color: "#666" }}>Loading…</div>
      ) : volumes.length === 0 ? (
        <div style={{ color: "#666" }}>No volumes configured.</div>
      ) : volumes.map((vol) => (
        <div key={`${vol.service_name}/${vol.volume_name}`} style={{
          background: "#111", borderRadius: 6, padding: "0.75rem 1rem", marginBottom: "0.75rem",
        }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
            <div>
              <span style={{ fontFamily: "monospace" }}>{vol.service_name}</span>
              <span style={{ color: "#666", margin: "0 0.4rem" }}>/</span>
              <span style={{ fontFamily: "monospace" }}>{vol.volume_name}</span>
              <span style={{ marginLeft: "0.75rem", color: vol.status === "active" ? "#86efac" : "#f87171", fontSize: "0.8rem" }}>
                {vol.status}
              </span>
            </div>
            <button
              onClick={() => openReorg(vol)}
              style={{ padding: "0.25rem 0.75rem", background: "#1a1a1a", color: "#eee", border: "1px solid #444", borderRadius: 4, cursor: "pointer", fontSize: "0.8rem" }}
            >
              Reorganize
            </button>
          </div>
          <div style={{ marginTop: "0.5rem", display: "flex", gap: "0.4rem", flexWrap: "wrap" }}>
            {vol.disk_paths.map((p, i) => (
              <span key={i} style={{ background: "#1a1a1a", border: "1px solid #333", borderRadius: 4, padding: "0.15rem 0.5rem", fontFamily: "monospace", fontSize: "0.75rem", color: "#aaa" }}>
                {i + 1}. {p}
              </span>
            ))}
          </div>
        </div>
      ))}

      {/* Reorganize modal */}
      {reorg && !confirmed && (
        <div style={{ position: "fixed", inset: 0, background: "rgba(0,0,0,0.6)", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 100 }}>
          <div style={{ background: "#1a1a1a", borderRadius: 8, padding: "1.25rem", width: "min(500px,95vw)" }}>
            <h3 style={{ marginTop: 0, fontSize: "1rem" }}>Reorganize disks</h3>
            <div style={{ marginBottom: "1rem" }}>
              {reorg.newPaths.map((p, i) => (
                <div key={i} style={{ display: "flex", alignItems: "center", gap: "0.5rem", marginBottom: "0.4rem" }}>
                  <span style={{ color: "#666", width: 20 }}>{i + 1}.</span>
                  <span style={{ fontFamily: "monospace", fontSize: "0.85rem", flex: 1, color: "#eee" }}>{p}</span>
                  <button onClick={() => moveUp(i)} disabled={i === 0} style={{ background: "none", border: "none", color: i === 0 ? "#444" : "#aaa", cursor: i === 0 ? "default" : "pointer", fontSize: "1rem" }}>↑</button>
                  <button onClick={() => moveDown(i)} disabled={i === reorg.newPaths.length - 1} style={{ background: "none", border: "none", color: i === reorg.newPaths.length - 1 ? "#444" : "#aaa", cursor: "pointer", fontSize: "1rem" }}>↓</button>
                </div>
              ))}
            </div>
            {estimate === null ? (
              <button onClick={fetchEstimate} style={{ padding: "0.4rem 0.9rem", background: "#333", color: "#eee", border: "none", borderRadius: 4, cursor: "pointer", marginRight: "0.5rem" }}>
                Estimate data movement
              </button>
            ) : (
              <div style={{ marginBottom: "0.75rem", fontSize: "0.9rem" }}>
                Data to move: <strong>{fmtBytes(estimate)}</strong>
              </div>
            )}
            <div style={{ display: "flex", gap: "0.5rem", marginTop: "0.75rem" }}>
              <button
                onClick={() => setConfirmed(true)}
                style={{ padding: "0.4rem 0.9rem", background: "#1a3a1a", color: "#86efac", border: "1px solid #86efac", borderRadius: 4, cursor: "pointer" }}
              >
                Confirm &amp; Reorganize
              </button>
              <button onClick={() => setReorg(null)} style={{ padding: "0.4rem 0.9rem", background: "#333", color: "#aaa", border: "none", borderRadius: 4, cursor: "pointer" }}>
                Cancel
              </button>
            </div>
          </div>
        </div>
      )}

      {reorg && confirmed && (
        <SseModal
          url={`/api/node-agent/volumes/${reorg.vol.service_name}/${reorg.vol.volume_name}/reorganize`}
          method="POST"
          body={{ new_disk_paths: reorg.newPaths }}
          onClose={() => { setReorg(null); setConfirmed(false); reload(); }}
        />
      )}
    </div>
  );
}

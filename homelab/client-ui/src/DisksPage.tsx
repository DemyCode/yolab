import { useEffect, useState } from "react";

interface Disk {
  name: string;
  model: string;
  size_bytes: number;
  used_bytes: number;
  mountpoints: string[];
  host: string;
}

function fmt(bytes: number): string {
  if (bytes === 0) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

function FillBar({ used, total }: { used: number; total: number }) {
  const pct = total > 0 ? Math.min(100, (used / total) * 100) : 0;
  const color = pct > 90 ? "#ef4444" : pct > 70 ? "#f59e0b" : "#22c55e";
  return (
    <div
      style={{
        background: "#e5e7eb",
        borderRadius: 4,
        height: 10,
        flex: 1,
        overflow: "hidden",
      }}
    >
      <div
        style={{ width: `${pct}%`, background: color, height: "100%", borderRadius: 4 }}
      />
    </div>
  );
}

export function DisksPage() {
  const [disks, setDisks] = useState<Disk[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    fetch("/api/disks")
      .then((r) => r.json())
      .then(setDisks)
      .catch((e) => setError(String(e)));
  }, []);

  if (error) return <div style={{ color: "red" }}>{error}</div>;
  if (!disks) return <div style={{ color: "#666" }}>Loading…</div>;
  if (disks.length === 0) return <div style={{ color: "#666" }}>No disks found.</div>;

  return (
    <table style={{ width: "100%", borderCollapse: "collapse", fontSize: "0.85rem" }}>
      <thead>
        <tr style={{ textAlign: "left", borderBottom: "2px solid #e5e7eb" }}>
          <th style={{ padding: "0.5rem 0.75rem" }}>Disk</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Host</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Size</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Used</th>
          <th style={{ padding: "0.5rem 0.75rem", minWidth: 200 }}>Usage</th>
        </tr>
      </thead>
      <tbody>
        {disks.map((d, i) => {
          const pct =
            d.size_bytes > 0 ? Math.round((d.used_bytes / d.size_bytes) * 100) : 0;
          return (
            <tr key={i} style={{ borderBottom: "1px solid #f3f4f6" }}>
              <td style={{ padding: "0.5rem 0.75rem" }}>
                <strong>{d.name}</strong>
                {d.model && (
                  <span style={{ color: "#888", marginLeft: "0.5rem", fontWeight: "normal" }}>
                    {d.model}
                  </span>
                )}
              </td>
              <td
                style={{
                  padding: "0.5rem 0.75rem",
                  fontFamily: "monospace",
                  fontSize: "0.75rem",
                  color: "#666",
                }}
              >
                {d.host}
              </td>
              <td style={{ padding: "0.5rem 0.75rem" }}>{fmt(d.size_bytes)}</td>
              <td style={{ padding: "0.5rem 0.75rem" }}>
                {d.used_bytes > 0 ? fmt(d.used_bytes) : "—"}
              </td>
              <td style={{ padding: "0.5rem 0.75rem" }}>
                {d.used_bytes > 0 ? (
                  <div style={{ display: "flex", alignItems: "center", gap: "0.5rem" }}>
                    <FillBar used={d.used_bytes} total={d.size_bytes} />
                    <span style={{ whiteSpace: "nowrap", fontSize: "0.75rem", color: "#666", minWidth: 32 }}>
                      {pct}%
                    </span>
                  </div>
                ) : (
                  "—"
                )}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

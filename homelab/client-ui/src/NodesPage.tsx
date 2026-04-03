import { useEffect, useState } from "react";

interface Node {
  name: string;
  ip: string;
  ready: boolean;
  roles: string[];
  joined_at: string;
}

export function NodesPage() {
  const [nodes, setNodes] = useState<Node[] | null>(null);

  useEffect(() => {
    fetch("/api/nodes")
      .then((r) => r.json())
      .then(setNodes)
      .catch(() => setNodes([]));
  }, []);

  if (!nodes) return <div style={{ color: "#666" }}>Loading…</div>;
  if (nodes.length === 0) return <div style={{ color: "#666" }}>No nodes found.</div>;

  return (
    <table style={{ width: "100%", borderCollapse: "collapse", fontSize: "0.85rem" }}>
      <thead>
        <tr style={{ textAlign: "left", borderBottom: "2px solid #e5e7eb" }}>
          <th style={{ padding: "0.5rem 0.75rem" }}>Name</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>IP</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Roles</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Status</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Joined</th>
        </tr>
      </thead>
      <tbody>
        {nodes.map((n) => (
          <tr key={n.name} style={{ borderBottom: "1px solid #f3f4f6" }}>
            <td style={{ padding: "0.5rem 0.75rem", fontWeight: "bold" }}>{n.name}</td>
            <td style={{ padding: "0.5rem 0.75rem", fontFamily: "monospace", fontSize: "0.78rem", color: "#666" }}>{n.ip}</td>
            <td style={{ padding: "0.5rem 0.75rem" }}>
              {n.roles.map((r) => (
                <span
                  key={r}
                  style={{
                    display: "inline-block",
                    background: "#f3f4f6",
                    borderRadius: 4,
                    padding: "0.1rem 0.4rem",
                    fontSize: "0.75rem",
                    marginRight: "0.25rem",
                  }}
                >
                  {r}
                </span>
              ))}
            </td>
            <td style={{ padding: "0.5rem 0.75rem" }}>
              <span style={{ color: n.ready ? "#22c55e" : "#ef4444", fontWeight: "bold" }}>
                {n.ready ? "Ready" : "Not Ready"}
              </span>
            </td>
            <td style={{ padding: "0.5rem 0.75rem", color: "#666", fontSize: "0.78rem" }}>
              {n.joined_at ? new Date(n.joined_at).toLocaleDateString() : "—"}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

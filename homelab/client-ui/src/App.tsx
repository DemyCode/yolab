import { useEffect, useRef, useState } from "react";

interface Status {
  commit_hash: string;
  commit_message: string;
  commit_date: string;
  platform: string;
  flake_target: string;
  error?: string;
}

function App() {
  const [status, setStatus] = useState<Status | null>(null);
  const [updating, setUpdating] = useState(false);
  const [log, setLog] = useState<string[]>([]);
  const [updateDone, setUpdateDone] = useState(false);
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    fetch("/api/status")
      .then((r) => r.json())
      .then(setStatus)
      .catch(() => setStatus(null));
  }, []);

  useEffect(() => {
    if (logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight;
    }
  }, [log]);

  async function runUpdate() {
    setUpdating(true);
    setLog([]);
    setUpdateDone(false);

    const response = await fetch("/api/update", { method: "POST" });
    if (!response.body) return;

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
        if (!line.trim()) continue;
        if (line === "[DONE]") {
          setUpdateDone(true);
          // Refresh status after a successful update
          fetch("/api/status").then((r) => r.json()).then(setStatus).catch(() => {});
        } else {
          setLog((prev) => [...prev, line]);
        }
      }
    }

    setUpdating(false);
  }

  const shortHash = status?.commit_hash?.slice(0, 8) ?? "—";
  const commitDate = status?.commit_date
    ? new Date(status.commit_date).toLocaleString()
    : "—";

  return (
    <div style={{ fontFamily: "monospace", maxWidth: 760, margin: "3rem auto", padding: "0 1rem" }}>
      <h1 style={{ fontSize: "1.6rem", marginBottom: "0.25rem" }}>YoLab</h1>
      <p style={{ color: "#666", marginTop: 0 }}>Your homelab is up and running.</p>

      <div style={{
        background: "#f5f5f5",
        borderRadius: 8,
        padding: "1rem 1.25rem",
        marginBottom: "1.5rem",
        fontSize: "0.9rem",
      }}>
        <div><strong>Platform:</strong> {status?.platform ?? "—"} ({status?.flake_target ?? "—"})</div>
        <div><strong>Commit:</strong> {shortHash} — {status?.commit_message ?? "—"}</div>
        <div><strong>Last built:</strong> {commitDate}</div>
        {status?.error && <div style={{ color: "red" }}><strong>Error:</strong> {status.error}</div>}
      </div>

      <button
        onClick={runUpdate}
        disabled={updating}
        style={{
          padding: "0.6rem 1.4rem",
          fontSize: "0.95rem",
          cursor: updating ? "not-allowed" : "pointer",
          background: updating ? "#999" : "#1a1a1a",
          color: "#fff",
          border: "none",
          borderRadius: 6,
        }}
      >
        {updating ? "Updating…" : "Update homelab"}
      </button>

      {(log.length > 0 || updateDone) && (
        <div style={{ marginTop: "1.25rem" }}>
          {updateDone && (
            <div style={{ color: "green", marginBottom: "0.5rem", fontWeight: "bold" }}>
              Update complete.
            </div>
          )}
          <div
            ref={logRef}
            style={{
              background: "#111",
              color: "#eee",
              borderRadius: 6,
              padding: "0.75rem 1rem",
              fontSize: "0.8rem",
              maxHeight: 400,
              overflowY: "auto",
              whiteSpace: "pre-wrap",
              wordBreak: "break-all",
            }}
          >
            {log.map((line, i) => (
              <div
                key={i}
                style={{
                  color: line.startsWith("[ERROR]")
                    ? "#f87171"
                    : line.startsWith("$")
                    ? "#86efac"
                    : "#eee",
                }}
              >
                {line}
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

export default App;

import { useEffect, useRef, useState } from "react";
import { DisksPage } from "./DisksPage";
import { NodesPage } from "./NodesPage";
import { RebuildPage } from "./RebuildPage";

interface Status {
  commit_hash: string;
  commit_message: string;
  commit_date: string;
  platform: string;
  flake_target: string;
  error?: string;
}

function OverviewPage() {
  const [status, setStatus] = useState<Status | null>(null);
  const [updating, setUpdating] = useState(false);
  const [log, setLog] = useState<string[]>([]);
  const [updateDone, setUpdateDone] = useState(false);
  const [reconnecting, setReconnecting] = useState(false);
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    fetch("/api/status")
      .then((r) => r.json())
      .then(setStatus)
      .catch(() => setStatus(null));

    fetch("/api/update/status")
      .then((r) => r.json())
      .then((d) => {
        if (d.running) {
          setUpdating(true);
          setLog(d.log ?? []);
          streamUpdateLog();
        }
      })
      .catch(() => {});
  }, []);

  async function streamUpdateLog() {
    const response = await fetch("/api/update/status");
    if (!response.ok) return;
    const d = await response.json();
    if (!d.running) {
      setUpdating(false);
      return;
    }
    const poll = async () => {
      await new Promise((r) => setTimeout(r, 1000));
      try {
        const r = await fetch("/api/update/status");
        const d = await r.json();
        setLog(d.log ?? []);
        if (!d.running) {
          setUpdating(false);
          const sawDone = (d.log ?? []).includes("[DONE]");
          if (sawDone) setUpdateDone(true);
          else pollUntilAlive();
          return;
        }
      } catch {
        setUpdating(false);
        pollUntilAlive();
        return;
      }
      poll();
    };
    poll();
  }

  useEffect(() => {
    if (logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight;
    }
  }, [log]);

  async function pollUntilAlive() {
    setReconnecting(true);
    for (let i = 0; i < 90; i++) {
      await new Promise((r) => setTimeout(r, 2000));
      try {
        const r = await fetch("/api/status");
        if (r.ok) {
          const s = await r.json();
          setStatus(s);
          setReconnecting(false);
          return;
        }
      } catch {}
    }
    setReconnecting(false);
  }

  async function runUpdate() {
    setUpdating(true);
    setLog([]);
    setUpdateDone(false);
    setReconnecting(false);

    try {
      const response = await fetch("/api/update", { method: "POST" });
      if (!response.body) return;

      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buf = "";
      let sawDone = false;

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
            sawDone = true;
            setUpdateDone(true);
            fetch("/api/status").then((r) => r.json()).then(setStatus).catch(() => {});
          } else {
            setLog((prev) => [...prev, line]);
          }
        }
      }

      // Stream ended without [DONE] — service restarted or machine is rebooting
      if (!sawDone) {
        setLog((prev) => [...prev, "[INFO] Connection lost — service is restarting or machine is rebooting…"]);
        pollUntilAlive();
      }
    } catch {
      setLog((prev) => [...prev, "[INFO] Connection lost — service is restarting or machine is rebooting…"]);
      pollUntilAlive();
    }

    setUpdating(false);
  }

  const shortHash = status?.commit_hash?.slice(0, 8) ?? "—";
  const commitDate = status?.commit_date
    ? new Date(status.commit_date).toLocaleString()
    : "—";

  return (
    <div>
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

      {(log.length > 0 || updateDone || reconnecting) && (
        <div style={{ marginTop: "1.25rem" }}>
          {updateDone && (
            <div style={{ color: "green", marginBottom: "0.5rem", fontWeight: "bold" }}>
              Update complete.
            </div>
          )}
          {reconnecting && (
            <div style={{ color: "#facc15", marginBottom: "0.5rem", fontWeight: "bold" }}>
              Waiting for service to come back online…
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

type Tab = "overview" | "nodes" | "disks" | "rebuild";

function App() {
  const [tab, setTab] = useState<Tab>("overview");

  return (
    <div style={{ fontFamily: "monospace", maxWidth: 900, margin: "3rem auto", padding: "0 1rem" }}>
      <h1 style={{ fontSize: "1.6rem", marginBottom: "0.25rem" }}>YoLab</h1>
      <p style={{ color: "#666", marginTop: 0, marginBottom: "1rem" }}>Your homelab is up and running.</p>
      <div style={{ display: "flex", gap: "0.25rem", marginBottom: "1.5rem", borderBottom: "2px solid #e5e7eb" }}>
        {(["overview", "nodes", "disks", "rebuild"] as Tab[]).map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            style={{
              background: "none",
              border: "none",
              borderBottom: tab === t ? "2px solid #1a1a1a" : "2px solid transparent",
              marginBottom: -2,
              padding: "0.5rem 0.75rem",
              cursor: "pointer",
              fontFamily: "monospace",
              fontSize: "0.9rem",
              fontWeight: tab === t ? "bold" : "normal",
            }}
          >
            {t.charAt(0).toUpperCase() + t.slice(1)}
          </button>
        ))}
      </div>
      {tab === "overview" && <OverviewPage />}
      {tab === "nodes" && <NodesPage />}
      {tab === "disks" && <DisksPage />}
      {tab === "rebuild" && <RebuildPage />}
    </div>
  );
}

export default App;

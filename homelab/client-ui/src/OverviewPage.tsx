import { useEffect, useRef, useState } from "react";

interface Status {
  commit_hash: string;
  commit_message: string;
  commit_date: string;
  platform: string;
  flake_target: string;
  error?: string;
}

export function OverviewPage() {
  const [status, setStatus] = useState<Status | null>(null);
  const [updating, setUpdating] = useState(false);
  const [updateLog, setUpdateLog] = useState<string[]>([]);
  const [updateDone, setUpdateDone] = useState(false);
  const [reconnecting, setReconnecting] = useState(false);
  const [rebuildLog, setRebuildLog] = useState<string[]>([]);
  const [rebuildRunning, setRebuildRunning] = useState(false);
  const updateLogRef = useRef<HTMLDivElement>(null);
  const rebuildLogRef = useRef<HTMLDivElement>(null);

  function loadRebuildLog(poll = false) {
    let cancelled = false;
    async function run() {
      try {
        const r = await fetch("/api/rebuild-log");
        const d = await r.json();
        if (cancelled) return;
        setRebuildLog(d.log ?? []);
        setRebuildRunning(d.running);
        if (d.running) setTimeout(run, 1500);
        else if (poll)
          fetch("/api/status")
            .then((r) => r.json())
            .then(setStatus)
            .catch(() => {});
      } catch {
        if (!cancelled) setTimeout(run, 3000);
      }
    }
    run();
    return () => {
      cancelled = true;
    };
  }

  async function pollUntilAlive() {
    setReconnecting(true);
    for (let i = 0; i < 90; i++) {
      await new Promise((r) => setTimeout(r, 2000));
      try {
        const r = await fetch("/api/status");
        if (r.ok) {
          setStatus(await r.json());
          setReconnecting(false);
          loadRebuildLog(true);
          return;
        }
      } catch {
        // continue retrying until server is back
      }
    }
    setReconnecting(false);
  }

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
        setUpdateLog(d.log ?? []);
        if (!d.running) {
          setUpdating(false);
          if ((d.log ?? []).includes("[DONE]")) setUpdateDone(true);
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
    fetch("/api/status")
      .then((r) => r.json())
      .then(setStatus)
      .catch(() => setStatus(null));
    fetch("/api/update/status")
      .then((r) => r.json())
      .then((d) => {
        if (d.running) {
          setUpdating(true);
          setUpdateLog(d.log ?? []);
          streamUpdateLog();
        }
      })
      .catch(() => {});
    loadRebuildLog();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (updateLogRef.current)
      updateLogRef.current.scrollTop = updateLogRef.current.scrollHeight;
  }, [updateLog]);

  useEffect(() => {
    if (rebuildLogRef.current)
      rebuildLogRef.current.scrollTop = rebuildLogRef.current.scrollHeight;
  }, [rebuildLog]);

  async function runUpdate() {
    setUpdating(true);
    setUpdateLog([]);
    setUpdateDone(false);
    setReconnecting(false);
    try {
      const response = await fetch("/api/update", { method: "POST" });
      if (!response.body) return;
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buf = "",
        sawDone = false;
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
            fetch("/api/status")
              .then((r) => r.json())
              .then(setStatus)
              .catch(() => {});
          } else setUpdateLog((prev) => [...prev, line]);
        }
      }
      if (!sawDone) {
        setUpdateLog((prev) => [
          ...prev,
          "[INFO] Connection lost — service is restarting…",
        ]);
        pollUntilAlive();
      }
    } catch {
      setUpdateLog((prev) => [
        ...prev,
        "[INFO] Connection lost — service is restarting…",
      ]);
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
      <div
        style={{
          background: "#f5f5f5",
          borderRadius: 8,
          padding: "1rem 1.25rem",
          marginBottom: "1.5rem",
          fontSize: "0.9rem",
        }}
      >
        <div>
          <strong>Platform:</strong> {status?.platform ?? "—"} (
          {status?.flake_target ?? "—"})
        </div>
        <div>
          <strong>Built commit:</strong> {shortHash} —{" "}
          {status?.commit_message ?? "—"}
        </div>
        <div>
          <strong>Built at:</strong> {commitDate}
        </div>
        {status?.error && (
          <div style={{ color: "red" }}>
            <strong>Error:</strong> {status.error}
          </div>
        )}
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
        {updating ? "Updating…" : "Update & rebuild"}
      </button>

      {(updateLog.length > 0 || updateDone || reconnecting) && (
        <div style={{ marginTop: "1.25rem" }}>
          <div
            style={{
              fontSize: "0.8rem",
              fontWeight: "bold",
              color: "#555",
              marginBottom: "0.4rem",
            }}
          >
            Update log
          </div>
          {updateDone && (
            <div
              style={{
                color: "green",
                marginBottom: "0.5rem",
                fontWeight: "bold",
                fontSize: "0.85rem",
              }}
            >
              Git update complete — rebuild running in background.
            </div>
          )}
          {reconnecting && (
            <div
              style={{
                color: "#facc15",
                marginBottom: "0.5rem",
                fontWeight: "bold",
                fontSize: "0.85rem",
              }}
            >
              Waiting for service to come back online…
            </div>
          )}
          <div
            ref={updateLogRef}
            style={{
              background: "#111",
              color: "#eee",
              borderRadius: 6,
              padding: "0.75rem 1rem",
              fontSize: "0.8rem",
              maxHeight: 200,
              overflowY: "auto",
              whiteSpace: "pre-wrap",
              wordBreak: "break-all",
            }}
          >
            {updateLog.map((line, i) => (
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

      {(rebuildLog.length > 0 || rebuildRunning) && (
        <div style={{ marginTop: "1.5rem" }}>
          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: "0.6rem",
              marginBottom: "0.4rem",
            }}
          >
            {rebuildRunning && (
              <span
                style={{
                  display: "inline-block",
                  width: 8,
                  height: 8,
                  borderRadius: "50%",
                  background: "#22c55e",
                  boxShadow: "0 0 6px #22c55e",
                  animation: "pulse 1.5s infinite",
                }}
              />
            )}
            <span
              style={{ fontSize: "0.8rem", fontWeight: "bold", color: "#555" }}
            >
              {rebuildRunning ? "Rebuild in progress…" : "Last rebuild log"}
            </span>
          </div>
          <div
            ref={rebuildLogRef}
            style={{
              background: "#111",
              color: "#d1fae5",
              borderRadius: 6,
              padding: "0.75rem 1rem",
              fontSize: "0.78rem",
              maxHeight: 400,
              overflowY: "auto",
              whiteSpace: "pre-wrap",
              wordBreak: "break-all",
              fontFamily: "monospace",
            }}
          >
            {rebuildLog.map((line, i) => (
              <div
                key={i}
                style={{
                  color:
                    line.includes("error:") || line.includes("Error")
                      ? "#f87171"
                      : line.startsWith("warning:")
                        ? "#fbbf24"
                        : "#d1fae5",
                }}
              >
                {line}
              </div>
            ))}
            {rebuildRunning && (
              <div style={{ color: "#6b7280", marginTop: "0.25rem" }}>▌</div>
            )}
          </div>
        </div>
      )}

      <style>{`@keyframes pulse { 0%, 100% { opacity: 1; } 50% { opacity: 0.4; } }`}</style>
    </div>
  );
}

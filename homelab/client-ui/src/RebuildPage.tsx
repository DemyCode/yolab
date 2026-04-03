import { useEffect, useRef, useState } from "react";

interface RebuildLog {
  running: boolean;
  log: string[];
}

export function RebuildPage() {
  const [data, setData] = useState<RebuildLog | null>(null);
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;

    async function poll() {
      try {
        const r = await fetch("/api/rebuild-log");
        const d: RebuildLog = await r.json();
        if (!cancelled) setData(d);
        if (!cancelled && d.running) setTimeout(poll, 1500);
      } catch {
        if (!cancelled) setTimeout(poll, 3000);
      }
    }

    poll();
    return () => { cancelled = true; };
  }, []);

  useEffect(() => {
    if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight;
  }, [data?.log.length]);

  if (!data) return <div style={{ color: "#666" }}>Loading…</div>;

  return (
    <div>
      <div style={{ display: "flex", alignItems: "center", gap: "0.75rem", marginBottom: "1rem" }}>
        {data.running ? (
          <>
            <span style={{
              display: "inline-block",
              width: 10,
              height: 10,
              borderRadius: "50%",
              background: "#22c55e",
              boxShadow: "0 0 6px #22c55e",
              animation: "pulse 1.5s infinite",
            }} />
            <span style={{ fontSize: "0.9rem" }}>Rebuild in progress…</span>
          </>
        ) : (
          <>
            <span style={{
              display: "inline-block",
              width: 10,
              height: 10,
              borderRadius: "50%",
              background: data.log.length > 0 ? "#6b7280" : "#374151",
            }} />
            <span style={{ fontSize: "0.9rem", color: "#666" }}>
              {data.log.length > 0 ? "Last rebuild log" : "No rebuild log yet"}
            </span>
            {data.log.length > 0 && (
              <button
                onClick={() => fetch("/api/rebuild-log").then(r => r.json()).then(setData)}
                style={{
                  marginLeft: "auto",
                  background: "none",
                  border: "1px solid #e5e7eb",
                  borderRadius: 4,
                  padding: "0.2rem 0.6rem",
                  fontSize: "0.8rem",
                  cursor: "pointer",
                }}
              >
                Refresh
              </button>
            )}
          </>
        )}
      </div>

      {data.log.length > 0 && (
        <div
          ref={logRef}
          style={{
            background: "#111",
            color: "#d1fae5",
            borderRadius: 6,
            padding: "0.75rem 1rem",
            fontSize: "0.78rem",
            maxHeight: 520,
            overflowY: "auto",
            whiteSpace: "pre-wrap",
            wordBreak: "break-all",
            fontFamily: "monospace",
          }}
        >
          {data.log.map((line, i) => (
            <div
              key={i}
              style={{
                color: line.includes("error:") || line.includes("Error")
                  ? "#f87171"
                  : line.startsWith("warning:")
                  ? "#fbbf24"
                  : "#d1fae5",
              }}
            >
              {line}
            </div>
          ))}
          {data.running && (
            <div style={{ color: "#6b7280", marginTop: "0.25rem" }}>▌</div>
          )}
        </div>
      )}

      <style>{`
        @keyframes pulse {
          0%, 100% { opacity: 1; }
          50% { opacity: 0.4; }
        }
      `}</style>
    </div>
  );
}

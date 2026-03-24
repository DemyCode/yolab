import { useEffect, useRef, useState } from "react";

interface Props {
  url: string;
  method?: "GET" | "POST";
  body?: object;
  onClose: () => void;
}

export function SseModal({ url, method = "POST", body, onClose }: Props) {
  const [log, setLog] = useState<string[]>([]);
  const [done, setDone] = useState(false);
  const [error, setError] = useState(false);
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;

    async function run() {
      try {
        const resp = await fetch(url, {
          method,
          headers: body ? { "Content-Type": "application/json" } : undefined,
          body: body ? JSON.stringify(body) : undefined,
        });
        if (!resp.body) return;
        const reader = resp.body.getReader();
        const decoder = new TextDecoder();
        let buf = "";
        while (true) {
          const { done: streamDone, value } = await reader.read();
          if (streamDone || cancelled) break;
          buf += decoder.decode(value, { stream: true });
          const parts = buf.split("\n\n");
          buf = parts.pop() ?? "";
          for (const part of parts) {
            const line = part.startsWith("data: ") ? part.slice(6) : part;
            if (!line.trim()) continue;
            if (line === "[DONE]") { setDone(true); return; }
            if (line.startsWith("[ERROR]")) setError(true);
            setLog((p) => [...p, line]);
          }
        }
      } catch (e) {
        if (!cancelled) setError(true);
      }
    }

    run();
    return () => { cancelled = true; };
  }, [url, method]);

  useEffect(() => {
    if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight;
  }, [log]);

  return (
    <div style={{
      position: "fixed", inset: 0, background: "rgba(0,0,0,0.6)",
      display: "flex", alignItems: "center", justifyContent: "center", zIndex: 100,
    }}>
      <div style={{
        background: "#1a1a1a", borderRadius: 8, padding: "1.25rem",
        width: "min(700px, 95vw)", maxHeight: "80vh", display: "flex", flexDirection: "column",
      }}>
        <div
          ref={logRef}
          style={{
            flex: 1, overflowY: "auto", background: "#111", borderRadius: 6,
            padding: "0.75rem 1rem", fontSize: "0.8rem", whiteSpace: "pre-wrap",
            wordBreak: "break-all", minHeight: 200, maxHeight: "60vh",
          }}
        >
          {log.map((line, i) => (
            <div key={i} style={{
              color: line.startsWith("[ERROR]") ? "#f87171"
                : line.startsWith("$") ? "#86efac"
                : "#eee",
            }}>{line}</div>
          ))}
          {!done && !error && log.length === 0 && (
            <div style={{ color: "#666" }}>Starting…</div>
          )}
        </div>
        {(done || error) && (
          <div style={{ marginTop: "0.75rem", display: "flex", alignItems: "center", gap: "1rem" }}>
            <span style={{ color: error ? "#f87171" : "#86efac", fontFamily: "monospace" }}>
              {error ? "Failed." : "Done."}
            </span>
            <button
              onClick={onClose}
              style={{
                marginLeft: "auto", padding: "0.4rem 1rem", background: "#333",
                color: "#fff", border: "none", borderRadius: 4, cursor: "pointer",
              }}
            >
              Close
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

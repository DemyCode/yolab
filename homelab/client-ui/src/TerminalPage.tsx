import { useEffect, useRef, useState } from "react";

interface Line {
  text: string;
  type: "input" | "output" | "error" | "exit";
}

export function TerminalPage() {
  const [lines, setLines] = useState<Line[]>([]);
  const [input, setInput] = useState("");
  const [running, setRunning] = useState(false);
  const [history, setHistory] = useState<string[]>([]);
  const [historyIdx, setHistoryIdx] = useState(-1);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [lines]);

  async function run(cmd: string) {
    if (!cmd.trim()) return;
    setHistory((h) => [cmd, ...h]);
    setHistoryIdx(-1);
    setLines((l) => [...l, { text: `$ ${cmd}`, type: "input" }]);
    setInput("");
    setRunning(true);

    try {
      const res = await fetch("/api/terminal/exec", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ command: cmd }),
      });
      const reader = res.body!.getReader();
      const decoder = new TextDecoder();
      let buf = "";

      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        const parts = buf.split("\n\n");
        buf = parts.pop() ?? "";
        for (const part of parts) {
          const text = part.startsWith("data: ") ? part.slice(6) : part;
          if (!text.trim()) continue;
          if (text.startsWith("[EXIT:")) {
            const code = text.slice(6, -1);
            if (code !== "0") {
              setLines((l) => [...l, { text: `[exited with code ${code}]`, type: "exit" }]);
            }
          } else if (text.startsWith("[ERROR]")) {
            setLines((l) => [...l, { text, type: "error" }]);
          } else {
            setLines((l) => [...l, { text, type: "output" }]);
          }
        }
      }
    } catch (e) {
      setLines((l) => [...l, { text: `[ERROR] ${e}`, type: "error" }]);
    }

    setRunning(false);
    setTimeout(() => inputRef.current?.focus(), 50);
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === "Enter") {
      run(input);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      const idx = Math.min(historyIdx + 1, history.length - 1);
      setHistoryIdx(idx);
      setInput(history[idx] ?? "");
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      const idx = Math.max(historyIdx - 1, -1);
      setHistoryIdx(idx);
      setInput(idx === -1 ? "" : history[idx] ?? "");
    }
  }

  return (
    <div
      onClick={() => inputRef.current?.focus()}
      style={{
        background: "#0d1117",
        borderRadius: 8,
        padding: "1rem",
        minHeight: 500,
        cursor: "text",
        fontFamily: "monospace",
        fontSize: "0.85rem",
      }}
    >
      {lines.map((l, i) => (
        <div
          key={i}
          style={{
            color: l.type === "input" ? "#86efac"
              : l.type === "error" ? "#f87171"
              : l.type === "exit" ? "#facc15"
              : "#e5e7eb",
            whiteSpace: "pre-wrap",
            wordBreak: "break-all",
            lineHeight: 1.5,
          }}
        >
          {l.text}
        </div>
      ))}
      <div style={{ display: "flex", alignItems: "center", marginTop: 2 }}>
        <span style={{ color: "#86efac", marginRight: 6 }}>$</span>
        <input
          ref={inputRef}
          autoFocus
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={onKeyDown}
          disabled={running}
          style={{
            background: "transparent",
            border: "none",
            outline: "none",
            color: "#e5e7eb",
            fontFamily: "monospace",
            fontSize: "0.85rem",
            flex: 1,
            caretColor: "#86efac",
          }}
        />
        {running && <span style={{ color: "#6b7280", marginLeft: 8 }}>running…</span>}
      </div>
      <div ref={bottomRef} />
    </div>
  );
}

import { useEffect, useRef, useState } from "react";
import { cn } from "@/lib/utils";

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
              setLines((l) => [...l, { text: `[exit ${code}]`, type: "exit" }]);
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
      void run(input);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      const idx = Math.min(historyIdx + 1, history.length - 1);
      setHistoryIdx(idx);
      setInput(history[idx] ?? "");
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      const idx = Math.max(historyIdx - 1, -1);
      setHistoryIdx(idx);
      setInput(idx === -1 ? "" : (history[idx] ?? ""));
    }
  }

  return (
    <div className="space-y-4">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Terminal</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Execute commands on your homelab
        </p>
      </div>

      <div
        onClick={() => inputRef.current?.focus()}
        className="rounded-xl border border-[#27272a] bg-[#09090b] p-4 min-h-[500px] cursor-text font-mono text-sm"
      >
        {lines.map((l, i) => (
          <div
            key={i}
            className={cn(
              "whitespace-pre-wrap break-all leading-6",
              l.type === "input" && "text-[#86efac]",
              l.type === "error" && "text-[#f87171]",
              l.type === "exit" && "text-[#fbbf24]",
              l.type === "output" && "text-[#e4e4e7]",
            )}
          >
            {l.text}
          </div>
        ))}

        <div className="flex items-center mt-1">
          <span className="text-[#86efac] mr-2 select-none">$</span>
          <input
            ref={inputRef}
            autoFocus
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={onKeyDown}
            disabled={running}
            className="flex-1 bg-transparent border-none outline-none text-[#e4e4e7] font-mono text-sm caret-[#86efac] placeholder-[#3f3f46]"
            placeholder={running ? "" : "type a command…"}
          />
          {running && (
            <span className="text-[#52525b] text-xs ml-2">running…</span>
          )}
        </div>
        <div ref={bottomRef} />
      </div>
    </div>
  );
}

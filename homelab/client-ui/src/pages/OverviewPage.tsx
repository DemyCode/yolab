import { useEffect, useRef, useState } from "react";
import {
  RefreshCw,
  GitCommit,
  Cpu,
  Calendar,
  AlertCircle,
  ChevronDown,
  Plus,
  Trash2,
  GitBranch,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import type { StatusInfo, RebuildLog, ChannelInfo } from "@/types/status";

type Phase = "idle" | "git" | "rebuild" | "done";

function LogLine({ line }: { line: string }) {
  const isError = line.startsWith("[ERROR]") || line.includes("error:");
  const isCmd = line.startsWith("$");
  const isWarning = line.startsWith("warning:");
  const isDim = line.startsWith("[INFO]");
  return (
    <div
      className={cn(
        "font-mono text-xs leading-5 whitespace-pre-wrap break-all",
        isError
          ? "text-[#f87171]"
          : isCmd
            ? "text-[#86efac]"
            : isWarning
              ? "text-[#fbbf24]"
              : isDim
                ? "text-[#52525b]"
                : "text-[#a1a1aa]",
      )}
    >
      {line}
    </div>
  );
}

export function OverviewPage() {
  const [status, setStatus] = useState<StatusInfo | null>(null);
  const [log, setLog] = useState<string[]>([]);
  const [phase, setPhase] = useState<Phase>("idle");
  const logRef = useRef<HTMLDivElement>(null);
  // Tracks how many rebuild-log lines we've already shown so we only append deltas
  const rebuildOffsetRef = useRef(0);

  // Channel state
  const [channel, setChannel] = useState<ChannelInfo | null>(null);
  const [channelOpen, setChannelOpen] = useState(false);
  const [editRemote, setEditRemote] = useState("");
  const [editRef, setEditRef] = useState("");
  const [newRemoteName, setNewRemoteName] = useState("");
  const [newRemoteUrl, setNewRemoteUrl] = useState("");
  const [addingRemote, setAddingRemote] = useState(false);
  const [channelSaving, setChannelSaving] = useState(false);

  useEffect(() => {
    if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight;
  }, [log]);

  function appendLines(lines: string[]) {
    if (lines.length > 0) setLog((prev) => [...prev, ...lines]);
  }

  // Poll /api/rebuild-log every 2 s. Handles service restarts transparently —
  // errors just retry. Stops when running=false (PID gone on the server).
  function pollRebuildLog() {
    let cancelled = false;
    async function tick() {
      if (cancelled) return;
      try {
        const r = await fetch("/api/rebuild-log");
        const d = (await r.json()) as RebuildLog;
        if (cancelled) return;
        const newLines = (d.log ?? []).slice(rebuildOffsetRef.current);
        rebuildOffsetRef.current = (d.log ?? []).length;
        appendLines(newLines);
        if (d.running) {
          setTimeout(tick, 2000);
        } else {
          setPhase("done");
          fetch("/api/status")
            .then((r) => r.json())
            .then((s) => setStatus(s as StatusInfo))
            .catch(() => {});
        }
      } catch {
        // Service is restarting — retry silently
        if (!cancelled) setTimeout(tick, 2000);
      }
    }
    tick();
    return () => {
      cancelled = true;
    };
  }

  function loadChannel() {
    fetch("/api/update/channel")
      .then((r) => r.json())
      .then((d: ChannelInfo) => {
        setChannel(d);
        setEditRemote(d.remote);
        setEditRef(d.ref);
      })
      .catch(() => {});
  }

  // On mount: check if a rebuild is already running from a previous session
  useEffect(() => {
    fetch("/api/status")
      .then((r) => r.json())
      .then((s) => setStatus(s as StatusInfo))
      .catch(() => setStatus(null));
    fetch("/api/rebuild-log")
      .then((r) => r.json())
      .then((d: RebuildLog) => {
        if (d.running) {
          setLog(d.log ?? []);
          rebuildOffsetRef.current = (d.log ?? []).length;
          setPhase("rebuild");
          pollRebuildLog();
        } else if ((d.log ?? []).length > 0) {
          setLog(d.log ?? []);
          setPhase("done");
        }
      })
      .catch(() => {});
    loadChannel();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  async function runUpdate() {
    setLog([]);
    setPhase("git");
    rebuildOffsetRef.current = 0;
    try {
      const response = await fetch("/api/update", { method: "POST" });
      if (response.body) {
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
            if (line.trim()) appendLines([line]);
          }
        }
      }
    } catch {
      /* service is restarting — handled by pollRebuildLog */
    }
    // Switch to rebuild phase and poll the PID-backed log directly.
    // Works whether the service restarted or not.
    setPhase("rebuild");
    rebuildOffsetRef.current = 0;
    pollRebuildLog();
  }

  async function saveChannelAndUpdate() {
    if (!editRemote.trim() || !editRef.trim()) return;
    setChannelSaving(true);
    try {
      await fetch("/api/update/channel", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          remote: editRemote.trim(),
          ref: editRef.trim(),
        }),
      });
      loadChannel();
      setChannelOpen(false);
    } finally {
      setChannelSaving(false);
    }
    void runUpdate();
  }

  async function handleAddRemote() {
    if (!newRemoteName.trim() || !newRemoteUrl.trim()) return;
    setAddingRemote(true);
    try {
      const r = await fetch("/api/update/remotes", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          name: newRemoteName.trim(),
          url: newRemoteUrl.trim(),
        }),
      });
      if (r.ok) {
        setNewRemoteName("");
        setNewRemoteUrl("");
        loadChannel();
      }
    } finally {
      setAddingRemote(false);
    }
  }

  async function handleRemoveRemote(name: string) {
    await fetch(`/api/update/remotes/${name}`, { method: "DELETE" });
    if (editRemote === name) setEditRemote("origin");
    loadChannel();
  }

  const updating = phase === "git" || phase === "rebuild";
  const shortHash = status?.commit_hash?.slice(0, 8) ?? "—";
  const commitDate = status?.commit_date
    ? new Date(status.commit_date).toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        year: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      })
    : "—";
  const channelLabel = channel
    ? `${channel.remote} / ${channel.ref}`
    : "origin / main";

  const logTitle =
    phase === "git"
      ? "Fetching & resetting…"
      : phase === "rebuild"
        ? "Building…"
        : "Last build";

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Overview</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          System status and build information
        </p>
      </div>

      {/* Status cards */}
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
        <Card>
          <CardContent className="flex items-start gap-3 pt-5">
            <div className="mt-0.5 rounded-md bg-[#a78bfa]/10 p-1.5">
              <Cpu className="h-4 w-4 text-[#a78bfa]" strokeWidth={1.75} />
            </div>
            <div className="min-w-0">
              <p className="text-xs text-[#71717a]">Platform</p>
              <p className="text-sm font-medium text-[#fafafa] truncate mt-0.5">
                {status?.platform ?? "—"}
              </p>
              <p className="text-xs text-[#52525b] truncate">
                {status?.flake_target ?? "—"}
              </p>
            </div>
          </CardContent>
        </Card>
        <Card>
          <CardContent className="flex items-start gap-3 pt-5">
            <div className="mt-0.5 rounded-md bg-[#a78bfa]/10 p-1.5">
              <GitCommit
                className="h-4 w-4 text-[#a78bfa]"
                strokeWidth={1.75}
              />
            </div>
            <div className="min-w-0">
              <p className="text-xs text-[#71717a]">Commit</p>
              <p className="text-sm font-medium text-[#fafafa] font-mono mt-0.5">
                {shortHash}
              </p>
              <p className="text-xs text-[#52525b] truncate">
                {status?.commit_message ?? "—"}
              </p>
            </div>
          </CardContent>
        </Card>
        <Card>
          <CardContent className="flex items-start gap-3 pt-5">
            <div className="mt-0.5 rounded-md bg-[#a78bfa]/10 p-1.5">
              <Calendar className="h-4 w-4 text-[#a78bfa]" strokeWidth={1.75} />
            </div>
            <div className="min-w-0">
              <p className="text-xs text-[#71717a]">Built at</p>
              <p className="text-sm font-medium text-[#fafafa] mt-0.5">
                {commitDate}
              </p>
            </div>
          </CardContent>
        </Card>
      </div>

      {status?.error && (
        <div className="flex items-start gap-2 rounded-lg border border-[#f87171]/30 bg-[#f87171]/5 p-4">
          <AlertCircle className="h-4 w-4 text-[#f87171] mt-0.5 flex-shrink-0" />
          <p className="text-sm text-[#f87171]">{status.error}</p>
        </div>
      )}

      {/* Update action + channel */}
      <Card>
        <CardContent className="pt-5 pb-4 space-y-4">
          <div className="flex items-center gap-3 flex-wrap">
            <Button
              onClick={() => void runUpdate()}
              disabled={updating}
              className="gap-2"
            >
              <RefreshCw
                className={cn("h-4 w-4", updating && "animate-spin")}
                strokeWidth={2}
              />
              {updating ? "Updating…" : "Update & rebuild"}
            </Button>
            <button
              onClick={() => setChannelOpen((o) => !o)}
              className="flex items-center gap-1.5 text-xs text-[#71717a] hover:text-[#a1a1aa] transition-colors"
            >
              <GitBranch className="h-3.5 w-3.5" />
              <span className="font-mono">{channelLabel}</span>
              <ChevronDown
                className={cn(
                  "h-3 w-3 transition-transform",
                  channelOpen && "rotate-180",
                )}
              />
            </button>
          </div>

          {channelOpen && (
            <div className="border-t border-[#27272a] pt-4 space-y-4">
              <div className="flex gap-2 flex-wrap">
                <div className="flex-1 min-w-[120px]">
                  <label className="text-xs text-[#71717a] mb-1 block">
                    Remote
                  </label>
                  <select
                    value={editRemote}
                    onChange={(e) => setEditRemote(e.target.value)}
                    className="w-full rounded-md border border-[#27272a] bg-[#09090b] text-[#fafafa] text-sm px-2.5 py-1.5 focus:outline-none focus:ring-1 focus:ring-[#a78bfa]"
                  >
                    {(channel?.remotes ?? []).map((r) => (
                      <option key={r.name} value={r.name}>
                        {r.name}
                      </option>
                    ))}
                  </select>
                </div>
                <div className="flex-1 min-w-[120px]">
                  <label className="text-xs text-[#71717a] mb-1 block">
                    Branch / tag / commit
                  </label>
                  <input
                    value={editRef}
                    onChange={(e) => setEditRef(e.target.value)}
                    placeholder="main"
                    className="w-full rounded-md border border-[#27272a] bg-[#09090b] text-[#fafafa] text-sm px-2.5 py-1.5 focus:outline-none focus:ring-1 focus:ring-[#a78bfa] font-mono"
                  />
                </div>
                <div className="flex items-end">
                  <Button
                    onClick={() => void saveChannelAndUpdate()}
                    disabled={
                      channelSaving ||
                      updating ||
                      !editRemote.trim() ||
                      !editRef.trim()
                    }
                    size="sm"
                  >
                    {channelSaving ? "Switching…" : "Switch & rebuild"}
                  </Button>
                </div>
              </div>

              {(channel?.remotes ?? []).length > 0 && (
                <div className="space-y-1">
                  <p className="text-xs text-[#52525b] uppercase tracking-wider font-semibold">
                    Remotes
                  </p>
                  {channel!.remotes.map((r) => (
                    <div
                      key={r.name}
                      className="flex items-center gap-2 text-xs"
                    >
                      <span className="font-mono text-[#a78bfa] w-24 truncate">
                        {r.name}
                      </span>
                      <span className="text-[#52525b] truncate flex-1">
                        {r.url}
                      </span>
                      {r.name !== "origin" && (
                        <button
                          onClick={() => void handleRemoveRemote(r.name)}
                          className="text-[#52525b] hover:text-[#f87171] transition-colors flex-shrink-0"
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                        </button>
                      )}
                    </div>
                  ))}
                </div>
              )}

              <div className="space-y-2">
                <p className="text-xs text-[#52525b] uppercase tracking-wider font-semibold">
                  Add remote
                </p>
                <div className="flex gap-2 flex-wrap">
                  <input
                    value={newRemoteName}
                    onChange={(e) => setNewRemoteName(e.target.value)}
                    placeholder="name"
                    className="w-28 rounded-md border border-[#27272a] bg-[#09090b] text-[#fafafa] text-sm px-2.5 py-1.5 focus:outline-none focus:ring-1 focus:ring-[#a78bfa] font-mono"
                  />
                  <input
                    value={newRemoteUrl}
                    onChange={(e) => setNewRemoteUrl(e.target.value)}
                    placeholder="https://github.com/user/yolab"
                    className="flex-1 min-w-[200px] rounded-md border border-[#27272a] bg-[#09090b] text-[#fafafa] text-sm px-2.5 py-1.5 focus:outline-none focus:ring-1 focus:ring-[#a78bfa]"
                  />
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => void handleAddRemote()}
                    disabled={
                      addingRemote ||
                      !newRemoteName.trim() ||
                      !newRemoteUrl.trim()
                    }
                    className="gap-1.5"
                  >
                    <Plus className="h-3.5 w-3.5" />
                    Add
                  </Button>
                </div>
              </div>
            </div>
          )}
        </CardContent>
      </Card>

      {/* Unified build log */}
      {phase !== "idle" && (
        <Card>
          <CardHeader>
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-2">
                {updating && (
                  <span className="inline-block w-2 h-2 rounded-full bg-[#4ade80] shadow-[0_0_6px_#4ade80] animate-pulse-dot" />
                )}
                <CardTitle>{logTitle}</CardTitle>
              </div>
              {phase === "done" && (
                <span className="text-xs text-[#4ade80] font-medium">
                  ✓ Done
                </span>
              )}
            </div>
          </CardHeader>
          <CardContent>
            <div
              ref={logRef}
              className="rounded-lg bg-[#09090b] border border-[#27272a] p-3 max-h-96 overflow-y-auto space-y-0.5"
            >
              {log.map((line, i) => (
                <LogLine key={i} line={line} />
              ))}
              {updating && (
                <div className="font-mono text-xs text-[#52525b]">▌</div>
              )}
            </div>
          </CardContent>
        </Card>
      )}
    </div>
  );
}

import { useEffect, useRef, useState } from "react";
import { RefreshCw, GitCommit, Cpu, Calendar, AlertCircle, ChevronDown, Plus, Trash2, GitBranch } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import type { StatusInfo, RebuildLog, ChannelInfo } from "@/types/status";

function LogLine({ line }: { line: string }) {
  const isError = line.startsWith("[ERROR]") || line.includes("error:");
  const isCmd = line.startsWith("$");
  const isWarning = line.startsWith("warning:");
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
              : "text-[#a1a1aa]",
      )}
    >
      {line}
    </div>
  );
}

export function OverviewPage() {
  const [status, setStatus] = useState<StatusInfo | null>(null);
  const [updating, setUpdating] = useState(false);
  const [updateLog, setUpdateLog] = useState<string[]>([]);
  const [updateDone, setUpdateDone] = useState(false);
  const [reconnecting, setReconnecting] = useState(false);
  const [rebuildLog, setRebuildLog] = useState<string[]>([]);
  const [rebuildRunning, setRebuildRunning] = useState(false);
  const updateLogRef = useRef<HTMLDivElement>(null);
  const rebuildLogRef = useRef<HTMLDivElement>(null);

  // Channel state
  const [channel, setChannel] = useState<ChannelInfo | null>(null);
  const [channelOpen, setChannelOpen] = useState(false);
  const [editRemote, setEditRemote] = useState("");
  const [editRef, setEditRef] = useState("");
  const [newRemoteName, setNewRemoteName] = useState("");
  const [newRemoteUrl, setNewRemoteUrl] = useState("");
  const [addingRemote, setAddingRemote] = useState(false);
  const [channelSaving, setChannelSaving] = useState(false);

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

  function loadRebuildLog(poll = false) {
    let cancelled = false;
    async function run() {
      try {
        const r = await fetch("/api/rebuild-log");
        const d = (await r.json()) as RebuildLog;
        if (cancelled) return;
        setRebuildLog(d.log ?? []);
        setRebuildRunning(d.running);
        if (d.running) setTimeout(run, 1500);
        else if (poll)
          fetch("/api/status")
            .then((r) => r.json())
            .then((s) => setStatus(s as StatusInfo))
            .catch(() => {});
      } catch {
        if (!cancelled) setTimeout(run, 3000);
      }
    }
    run();
    return () => { cancelled = true; };
  }

  async function pollUntilAlive() {
    setReconnecting(true);
    for (let i = 0; i < 90; i++) {
      await new Promise((r) => setTimeout(r, 2000));
      try {
        const r = await fetch("/api/status");
        if (r.ok) {
          setStatus((await r.json()) as StatusInfo);
          setReconnecting(false);
          loadRebuildLog(true);
          return;
        }
      } catch {
        // retry
      }
    }
    setReconnecting(false);
  }

  async function streamUpdateLog() {
    const response = await fetch("/api/rebuild-log");
    if (!response.ok) return;
    const d = (await response.json()) as { running: boolean };
    if (!d.running) { setUpdating(false); return; }
    const poll = async () => {
      await new Promise((r) => setTimeout(r, 1000));
      try {
        const r = await fetch("/api/rebuild-log");
        const d = (await r.json()) as RebuildLog;
        setUpdateLog(d.log ?? []);
        if (!d.running) {
          setUpdating(false);
          if ((d.log ?? []).includes("[DONE]")) setUpdateDone(true);
          else void pollUntilAlive();
          return;
        }
      } catch {
        setUpdating(false);
        void pollUntilAlive();
        return;
      }
      void poll();
    };
    void poll();
  }

  useEffect(() => {
    fetch("/api/status")
      .then((r) => r.json())
      .then((s) => setStatus(s as StatusInfo))
      .catch(() => setStatus(null));
    fetch("/api/rebuild-log")
      .then((r) => r.json())
      .then((d: RebuildLog) => {
        if (d.running) {
          setUpdating(true);
          setUpdateLog(d.log ?? []);
          streamUpdateLog();
        }
      })
      .catch(() => {});
    loadRebuildLog();
    loadChannel();
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
      let buf = "", sawDone = false;
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
            fetch("/api/status").then((r) => r.json()).then((s) => setStatus(s as StatusInfo)).catch(() => {});
          } else setUpdateLog((prev) => [...prev, line]);
        }
      }
      if (!sawDone) {
        setUpdateLog((prev) => [...prev, "[INFO] Connection lost — service is restarting…"]);
        void pollUntilAlive();
      }
    } catch {
      setUpdateLog((prev) => [...prev, "[INFO] Connection lost — service is restarting…"]);
      void pollUntilAlive();
    }
    setUpdating(false);
  }

  async function saveChannelAndUpdate() {
    if (!editRemote.trim() || !editRef.trim()) return;
    setChannelSaving(true);
    try {
      await fetch("/api/update/channel", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ remote: editRemote.trim(), ref: editRef.trim() }),
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
        body: JSON.stringify({ name: newRemoteName.trim(), url: newRemoteUrl.trim() }),
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

  const shortHash = status?.commit_hash?.slice(0, 8) ?? "—";
  const commitDate = status?.commit_date
    ? new Date(status.commit_date).toLocaleString(undefined, {
        month: "short", day: "numeric", year: "numeric",
        hour: "2-digit", minute: "2-digit",
      })
    : "—";

  const channelLabel = channel
    ? `${channel.remote} / ${channel.ref}`
    : "origin / main";

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Overview</h1>
        <p className="text-sm text-[#71717a] mt-0.5">System status and build information</p>
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
              <p className="text-sm font-medium text-[#fafafa] truncate mt-0.5">{status?.platform ?? "—"}</p>
              <p className="text-xs text-[#52525b] truncate">{status?.flake_target ?? "—"}</p>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardContent className="flex items-start gap-3 pt-5">
            <div className="mt-0.5 rounded-md bg-[#a78bfa]/10 p-1.5">
              <GitCommit className="h-4 w-4 text-[#a78bfa]" strokeWidth={1.75} />
            </div>
            <div className="min-w-0">
              <p className="text-xs text-[#71717a]">Commit</p>
              <p className="text-sm font-medium text-[#fafafa] font-mono mt-0.5">{shortHash}</p>
              <p className="text-xs text-[#52525b] truncate">{status?.commit_message ?? "—"}</p>
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
              <p className="text-sm font-medium text-[#fafafa] mt-0.5">{commitDate}</p>
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
            <Button onClick={() => void runUpdate()} disabled={updating} className="gap-2">
              <RefreshCw className={cn("h-4 w-4", updating && "animate-spin")} strokeWidth={2} />
              {updating ? "Updating…" : "Update & rebuild"}
            </Button>

            <button
              onClick={() => setChannelOpen((o) => !o)}
              className="flex items-center gap-1.5 text-xs text-[#71717a] hover:text-[#a1a1aa] transition-colors"
            >
              <GitBranch className="h-3.5 w-3.5" />
              <span className="font-mono">{channelLabel}</span>
              <ChevronDown className={cn("h-3 w-3 transition-transform", channelOpen && "rotate-180")} />
            </button>
          </div>

          {channelOpen && (
            <div className="border-t border-[#27272a] pt-4 space-y-4">
              {/* Remote + ref selectors */}
              <div className="flex gap-2 flex-wrap">
                <div className="flex-1 min-w-[120px]">
                  <label className="text-xs text-[#71717a] mb-1 block">Remote</label>
                  <select
                    value={editRemote}
                    onChange={(e) => setEditRemote(e.target.value)}
                    className="w-full rounded-md border border-[#27272a] bg-[#09090b] text-[#fafafa] text-sm px-2.5 py-1.5 focus:outline-none focus:ring-1 focus:ring-[#a78bfa]"
                  >
                    {(channel?.remotes ?? []).map((r) => (
                      <option key={r.name} value={r.name}>{r.name}</option>
                    ))}
                  </select>
                </div>
                <div className="flex-1 min-w-[120px]">
                  <label className="text-xs text-[#71717a] mb-1 block">Branch / tag / commit</label>
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
                    disabled={channelSaving || updating || !editRemote.trim() || !editRef.trim()}
                    size="sm"
                  >
                    {channelSaving ? "Switching…" : "Switch & rebuild"}
                  </Button>
                </div>
              </div>

              {/* Remotes list */}
              {(channel?.remotes ?? []).length > 0 && (
                <div className="space-y-1">
                  <p className="text-xs text-[#52525b] uppercase tracking-wider font-semibold">Remotes</p>
                  {channel!.remotes.map((r) => (
                    <div key={r.name} className="flex items-center gap-2 text-xs">
                      <span className="font-mono text-[#a78bfa] w-24 truncate">{r.name}</span>
                      <span className="text-[#52525b] truncate flex-1">{r.url}</span>
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

              {/* Add remote */}
              <div className="space-y-2">
                <p className="text-xs text-[#52525b] uppercase tracking-wider font-semibold">Add remote</p>
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
                    disabled={addingRemote || !newRemoteName.trim() || !newRemoteUrl.trim()}
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

      {/* Update log */}
      {(updateLog.length > 0 || updateDone || reconnecting) && (
        <Card>
          <CardHeader>
            <div className="flex items-center justify-between">
              <CardTitle>Update log</CardTitle>
              {updateDone && <span className="text-xs text-[#4ade80] font-medium">✓ Git update complete</span>}
              {reconnecting && <span className="text-xs text-[#fbbf24] font-medium">Waiting for service…</span>}
            </div>
          </CardHeader>
          <CardContent>
            <div
              ref={updateLogRef}
              className="rounded-lg bg-[#09090b] border border-[#27272a] p-3 max-h-52 overflow-y-auto space-y-0.5"
            >
              {updateLog.map((line, i) => <LogLine key={i} line={line} />)}
            </div>
          </CardContent>
        </Card>
      )}

      {/* Rebuild log */}
      {(rebuildLog.length > 0 || rebuildRunning) && (
        <Card>
          <CardHeader>
            <div className="flex items-center gap-2">
              {rebuildRunning && (
                <span className="inline-block w-2 h-2 rounded-full bg-[#4ade80] shadow-[0_0_6px_#4ade80] animate-pulse-dot" />
              )}
              <CardTitle>{rebuildRunning ? "Rebuild in progress…" : "Last rebuild log"}</CardTitle>
            </div>
          </CardHeader>
          <CardContent>
            <div
              ref={rebuildLogRef}
              className="rounded-lg bg-[#09090b] border border-[#27272a] p-3 max-h-96 overflow-y-auto space-y-0.5"
            >
              {rebuildLog.map((line, i) => <LogLine key={i} line={line} />)}
              {rebuildRunning && <div className="font-mono text-xs text-[#52525b]">▌</div>}
            </div>
          </CardContent>
        </Card>
      )}
    </div>
  );
}

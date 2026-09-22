import { useEffect, useRef, useState } from "react";
import {
  RefreshCw,
  GitCommit,
  Cpu,
  Calendar,
  AlertCircle,
  ChevronDown,
  GitBranch,
  Power,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import { streamEvents } from "@/lib/api";
import type { StatusInfo, RebuildLog, ChannelInfo } from "@/types/status";
import { NotificationsCard } from "@/components/NotificationsCard";

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
          ? "text-danger"
          : isCmd
            ? "text-success"
            : isWarning
              ? "text-warning"
              : isDim
                ? "text-fg-subtle"
                : "text-fg-muted",
      )}
    >
      {line}
    </div>
  );
}

export function SystemPage() {
  const [status, setStatus] = useState<StatusInfo | null>(null);
  const [log, setLog] = useState<string[]>([]);
  const [phase, setPhase] = useState<Phase>("idle");
  const logRef = useRef<HTMLDivElement>(null);
  const rebuildOffsetRef = useRef(0);

  const [channel, setChannel] = useState<ChannelInfo | null>(null);
  const [channelOpen, setChannelOpen] = useState(false);
  const [editUrl, setEditUrl] = useState("");
  const [editRef, setEditRef] = useState("");
  const [channelSaving, setChannelSaving] = useState(false);

  const [rebootConfirm, setRebootConfirm] = useState(false);
  const [rebooting, setRebooting] = useState(false);

  useEffect(() => {
    if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight;
  }, [log]);

  function appendLines(lines: string[]) {
    if (lines.length > 0) setLog((prev) => [...prev, ...lines]);
  }

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
        setEditUrl(d.url);
        setEditRef(d.ref);
      })
      .catch(() => {});
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

  async function streamUpdate(url: string) {
    setLog([]);
    setPhase("git");
    rebuildOffsetRef.current = 0;
    try {
      await streamEvents(url, { method: "POST" }, (line) =>
        appendLines([line]),
      );
    } catch {}
    setPhase("rebuild");
    rebuildOffsetRef.current = 0;
    pollRebuildLog();
  }

  const runUpdate = () => streamUpdate("/api/update");
  const runUpdateAll = () => streamUpdate("/api/update/all");

  async function rebootAll() {
    setRebooting(true);
    setRebootConfirm(false);
    try {
      await fetch("/api/system/reboot/all", { method: "POST" });
    } catch {}
  }

  async function saveChannelAndUpdate() {
    if (!editUrl.trim() || !editRef.trim()) return;
    setChannelSaving(true);
    try {
      await fetch("/api/update/channel", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          url: editUrl.trim(),
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

  const updating = phase === "git" || phase === "rebuild";
  const shortHash = status?.commit_hash?.slice(0, 8) || "—";
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
    ? `${channel.url} / ${channel.ref}`
    : "github:DemyCode/yolab / main";

  const logTitle =
    phase === "git"
      ? "Fetching & resetting…"
      : phase === "rebuild"
        ? "Building…"
        : "Last build";

  return (
    <div className="space-y-6 max-w-3xl">
      {}
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
        <Card>
          <CardContent className="flex items-start gap-3 pt-5">
            <div className="mt-0.5 rounded-md bg-primary/10 p-1.5">
              <Cpu className="h-4 w-4 text-primary" strokeWidth={1.75} />
            </div>
            <div className="min-w-0">
              <p className="text-xs text-fg-muted">Platform</p>
              <p className="text-sm font-medium text-fg truncate mt-0.5">
                {status?.platform ?? "—"}
              </p>
              <p className="text-xs text-fg-subtle truncate">
                {status?.flake_target ?? "—"}
              </p>
            </div>
          </CardContent>
        </Card>
        <Card>
          <CardContent className="flex items-start gap-3 pt-5">
            <div className="mt-0.5 rounded-md bg-primary/10 p-1.5">
              <GitCommit className="h-4 w-4 text-primary" strokeWidth={1.75} />
            </div>
            <div className="min-w-0">
              <p className="text-xs text-fg-muted">Commit</p>
              <p className="text-sm font-medium text-fg font-mono mt-0.5">
                {shortHash}
              </p>
              <p className="text-xs text-fg-subtle truncate">
                {status?.commit_message || "—"}
              </p>
            </div>
          </CardContent>
        </Card>
        <Card>
          <CardContent className="flex items-start gap-3 pt-5">
            <div className="mt-0.5 rounded-md bg-primary/10 p-1.5">
              <Calendar className="h-4 w-4 text-primary" strokeWidth={1.75} />
            </div>
            <div className="min-w-0">
              <p className="text-xs text-fg-muted">Built at</p>
              <p className="text-sm font-medium text-fg mt-0.5">{commitDate}</p>
            </div>
          </CardContent>
        </Card>
      </div>

      {status?.error && (
        <div className="flex items-start gap-2 rounded-lg border border-danger/30 bg-danger/5 p-4">
          <AlertCircle className="h-4 w-4 text-danger mt-0.5 flex-shrink-0" />
          <p className="text-sm text-danger">{status.error}</p>
        </div>
      )}

      <NotificationsCard />

      {}
      <Card>
        <CardContent className="pt-5 pb-4 space-y-4">
          <div className="flex items-center gap-3 flex-wrap">
            <Button
              onClick={() => void runUpdateAll()}
              disabled={updating}
              className="gap-2"
            >
              <RefreshCw
                className={cn("h-4 w-4", updating && "animate-spin")}
                strokeWidth={2}
              />
              {updating ? "Updating…" : "Update all machines"}
            </Button>

            {}
            {rebootConfirm ? (
              <div className="flex items-center gap-2">
                <Button
                  variant="danger"
                  onClick={() => void rebootAll()}
                  disabled={rebooting}
                  className="gap-2"
                >
                  <Power className="h-4 w-4" strokeWidth={2} />
                  Reboot everything now
                </Button>
                <Button variant="ghost" onClick={() => setRebootConfirm(false)}>
                  Cancel
                </Button>
              </div>
            ) : (
              <Button
                variant="ghost"
                onClick={() => setRebootConfirm(true)}
                disabled={updating || rebooting}
                className="gap-2"
              >
                <Power className="h-4 w-4" strokeWidth={2} />
                {rebooting ? "Rebooting…" : "Reboot all machines"}
              </Button>
            )}
            <button
              onClick={() => setChannelOpen((o) => !o)}
              className="flex items-center gap-1.5 text-xs text-fg-muted hover:text-fg-muted transition-colors"
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
            <div className="border-t border-border pt-4 space-y-4">
              <div className="flex gap-2 flex-wrap">
                <div className="flex-[2] min-w-[220px]">
                  <label className="text-xs text-fg-muted mb-1 block">
                    Flake source
                  </label>
                  <input
                    value={editUrl}
                    onChange={(e) => setEditUrl(e.target.value)}
                    placeholder="github:DemyCode/yolab"
                    className="w-full rounded-md border border-border bg-bg text-fg text-sm px-2.5 py-1.5 focus:outline-none focus:ring-1 focus:ring-primary font-mono"
                  />
                </div>
                <div className="flex-1 min-w-[120px]">
                  <label className="text-xs text-fg-muted mb-1 block">
                    Branch / tag / commit
                  </label>
                  <input
                    value={editRef}
                    onChange={(e) => setEditRef(e.target.value)}
                    placeholder="main"
                    className="w-full rounded-md border border-border bg-bg text-fg text-sm px-2.5 py-1.5 focus:outline-none focus:ring-1 focus:ring-primary font-mono"
                  />
                </div>
                <div className="flex items-end">
                  <Button
                    onClick={() => void saveChannelAndUpdate()}
                    disabled={
                      channelSaving ||
                      updating ||
                      !editUrl.trim() ||
                      !editRef.trim()
                    }
                    size="sm"
                  >
                    {channelSaving ? "Switching…" : "Switch & rebuild"}
                  </Button>
                </div>
              </div>

              <p className="text-xs text-fg-subtle">
                This machine builds from the flake URL, not a checkout — its own
                config.toml is the only local file. A community fork is the same
                shape: point it at its own URL.
              </p>
            </div>
          )}
        </CardContent>
      </Card>

      {}
      {phase !== "idle" && (
        <Card>
          <CardHeader>
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-2">
                {updating && (
                  <span className="inline-block w-2 h-2 rounded-full bg-success shadow-[0_0_6px_#4ade80] animate-pulse-dot" />
                )}
                <CardTitle>{logTitle}</CardTitle>
              </div>
              {phase === "done" && (
                <span className="text-xs text-success font-medium">✓ Done</span>
              )}
            </div>
          </CardHeader>
          <CardContent>
            <div
              ref={logRef}
              className="rounded-lg bg-bg border border-border p-3 max-h-96 overflow-y-auto space-y-0.5"
            >
              {log.map((line, i) => (
                <LogLine key={i} line={line} />
              ))}
              {updating && (
                <div className="font-mono text-xs text-fg-subtle">▌</div>
              )}
            </div>
          </CardContent>
        </Card>
      )}
    </div>
  );
}

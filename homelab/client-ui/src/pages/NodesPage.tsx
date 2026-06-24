import { useEffect, useState, useCallback } from "react";
import { ExternalLink, AlertTriangle, ArrowDown, ArrowUp, RefreshCw } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
  CardDescription,
} from "@/components/ui/card";
import type { NodeInfo, NodeLink, TrafficData, TrafficPoint } from "@/types/nodes";

function fmtBytes(bytes: number): string {
  if (bytes >= 1e12) return (bytes / 1e12).toFixed(2) + " TB";
  if (bytes >= 1e9) return (bytes / 1e9).toFixed(2) + " GB";
  if (bytes >= 1e6) return (bytes / 1e6).toFixed(1) + " MB";
  if (bytes >= 1e3) return (bytes / 1e3).toFixed(0) + " KB";
  return bytes + " B";
}

function fmtDate(iso: string, mode: "hour" | "day" | "month"): string {
  const d = new Date(iso);
  if (mode === "hour") return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (mode === "day") return d.toLocaleDateString([], { month: "short", day: "numeric" });
  return d.toLocaleDateString([], { month: "short", year: "numeric" });
}

function BarChart({ points, mode }: { points: TrafficPoint[]; mode: "hour" | "day" | "month" }) {
  const maxVal = Math.max(...points.map((p) => p.rx + p.tx), 1);
  const showCount = mode === "hour" ? 24 : mode === "day" ? 30 : 12;

  if (points.length === 0) {
    return (
      <div className="h-24 flex items-center justify-center text-xs text-[#3f3f46]">
        No data yet
      </div>
    );
  }

  return (
    <div className="flex items-end gap-0.5 h-20 w-full">
      {points.slice(-showCount).map((p, i) => {
        const total = p.rx + p.tx;
        const heightPct = (total / maxVal) * 100;
        const rxPct = total > 0 ? (p.rx / total) * 100 : 50;
        return (
          <div
            key={i}
            className="flex-1 flex flex-col justify-end group relative"
            title={`${fmtDate(p.ts, mode)}: ↓${fmtBytes(p.rx)} ↑${fmtBytes(p.tx)}`}
          >
            <div style={{ height: `${heightPct}%` }} className="flex flex-col justify-end rounded-sm overflow-hidden min-h-[2px]">
              <div style={{ height: `${100 - rxPct}%` }} className="bg-[#f97316]/70 min-h-[1px]" />
              <div style={{ height: `${rxPct}%` }} className="bg-[#60a5fa]/70 min-h-[1px]" />
            </div>
          </div>
        );
      })}
    </div>
  );
}

function trafficAsMonthlyPoints(monthly: { year_month: string; bytes: number }[]): TrafficPoint[] {
  return [...monthly].reverse().map((m) => ({ ts: m.year_month + "-01T00:00:00Z", rx: m.bytes, tx: 0 }));
}

const CACHE_KEY = "yolab:nodes";

type TrafficView = "hourly" | "daily" | "monthly";

export function NodesPage() {
  const [nodes, setNodes] = useState<NodeInfo[] | null>(null);
  const [links, setLinks] = useState<NodeLink[]>([]);
  const [stale, setStale] = useState(false);
  const [traffic, setTraffic] = useState<TrafficData | null>(null);
  const [trafficErr, setTrafficErr] = useState<string | null>(null);
  const [trafficView, setTrafficView] = useState<TrafficView>("daily");
  const [trafficLoading, setTrafficLoading] = useState(false);

  const fetchTraffic = useCallback(() => {
    setTrafficLoading(true);
    fetch("/api/nodes/traffic")
      .then((r) => r.json())
      .then((d: TrafficData & { error?: string }) => {
        if (d.error) {
          setTrafficErr(d.error);
        } else {
          setTraffic(d);
          setTrafficErr(null);
        }
      })
      .catch((e) => setTrafficErr(String(e)))
      .finally(() => setTrafficLoading(false));
  }, []);

  useEffect(() => {
    try {
      const cached = localStorage.getItem(CACHE_KEY);
      if (cached) {
        setNodes(JSON.parse(cached) as NodeInfo[]);
        setStale(true);
      }
    } catch {}

    fetch("/api/nodes")
      .then((r) => r.json())
      .then((n: NodeInfo[]) => {
        if (n.length > 0) {
          localStorage.setItem(CACHE_KEY, JSON.stringify(n));
          setNodes(n);
          setStale(false);
        } else {
          setStale(true);
          setNodes((prev) => prev ?? []);
        }
      })
      .catch(() => {
        setStale(true);
        setNodes((prev) => prev ?? []);
      });

    fetch("/api/nodes/links")
      .then((r) => r.json())
      .then((l: NodeLink[]) => setLinks((prev) => (l.length > 0 ? l : prev)))
      .catch(() => {});

    fetchTraffic();
    const id = setInterval(fetchTraffic, 60_000);
    return () => clearInterval(id);
  }, [fetchTraffic]);

  const urlFor = (name: string) =>
    links.find((l) => l.name === name)?.url ?? null;

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Machines</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Machines in your cluster
        </p>
      </div>

      {stale && (
        <div className="flex items-start gap-2.5 rounded-lg border border-[#fbbf24]/30 bg-[#fbbf24]/5 px-4 py-3">
          <AlertTriangle className="h-4 w-4 text-[#fbbf24] mt-0.5 flex-shrink-0" />
          <p className="text-sm text-[#fbbf24]">
            Cluster API unreachable — showing last known state. One or more machines may be offline.
          </p>
        </div>
      )}

      {/* Network Traffic */}
      <Card>
        <CardHeader>
          <div className="flex items-start justify-between gap-4">
            <div>
              <CardTitle>Network traffic</CardTitle>
              <CardDescription>
                WireGuard bandwidth from the relay server
                {traffic && (
                  <span className="ml-2 text-[#fafafa]/60">
                    — {fmtBytes(traffic.current_month.bytes)} this month ({traffic.current_month.year_month})
                  </span>
                )}
              </CardDescription>
            </div>
            <button
              onClick={fetchTraffic}
              disabled={trafficLoading}
              className="text-[#71717a] hover:text-[#fafafa] transition-colors flex-shrink-0 mt-0.5"
              title="Refresh"
            >
              <RefreshCw className={`h-3.5 w-3.5 ${trafficLoading ? "animate-spin" : ""}`} />
            </button>
          </div>
        </CardHeader>
        <CardContent className="space-y-5">
          {trafficErr ? (
            <p className="text-xs text-[#f87171]">{trafficErr}</p>
          ) : !traffic ? (
            <p className="text-sm text-[#71717a]">Loading…</p>
          ) : (
            <>
              {/* Per-node breakdown */}
              {traffic.nodes.length > 0 && (
                <div className="space-y-2">
                  {traffic.nodes.map((n) => (
                    <div key={n.node_id} className="flex items-center gap-3 text-sm">
                      <span className="font-mono text-xs text-[#a1a1aa] flex-1 truncate">
                        {n.sub_ipv6}
                      </span>
                      <span className="flex items-center gap-1 text-xs text-[#60a5fa]">
                        <ArrowDown className="h-3 w-3" />
                        {fmtBytes(n.rx_bytes)}
                      </span>
                      <span className="flex items-center gap-1 text-xs text-[#f97316]">
                        <ArrowUp className="h-3 w-3" />
                        {fmtBytes(n.tx_bytes)}
                      </span>
                      {n.last_seen && (
                        <span className="text-xs text-[#3f3f46] hidden sm:block">
                          {new Date(n.last_seen).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
                        </span>
                      )}
                    </div>
                  ))}
                </div>
              )}

              {/* Chart view toggle */}
              <div className="flex items-center gap-1 border-b border-[#27272a] pb-3">
                {(["hourly", "daily", "monthly"] as TrafficView[]).map((v) => (
                  <button
                    key={v}
                    onClick={() => setTrafficView(v)}
                    className={`px-2.5 py-1 rounded text-xs transition-colors ${
                      trafficView === v
                        ? "bg-[#a78bfa]/15 text-[#a78bfa]"
                        : "text-[#71717a] hover:text-[#fafafa]"
                    }`}
                  >
                    {v === "hourly" ? "Last 24h" : v === "daily" ? "Last 30d" : "Monthly"}
                  </button>
                ))}
                <div className="ml-auto flex items-center gap-3 text-xs text-[#71717a]">
                  <span className="flex items-center gap-1">
                    <span className="inline-block w-2.5 h-2.5 rounded-sm bg-[#60a5fa]/70" />
                    Download
                  </span>
                  <span className="flex items-center gap-1">
                    <span className="inline-block w-2.5 h-2.5 rounded-sm bg-[#f97316]/70" />
                    Upload
                  </span>
                </div>
              </div>

              {trafficView === "hourly" && (
                <BarChart points={traffic.hourly} mode="hour" />
              )}
              {trafficView === "daily" && (
                <BarChart points={traffic.daily} mode="day" />
              )}
              {trafficView === "monthly" && (
                <BarChart points={trafficAsMonthlyPoints(traffic.monthly)} mode="month" />
              )}

              {/* Monthly history table */}
              {traffic.monthly.length > 0 && (
                <div className="space-y-1 pt-1">
                  <p className="text-xs font-medium text-[#71717a] uppercase tracking-wider">History</p>
                  <div className="space-y-1">
                    {traffic.monthly.map((m) => {
                      const maxBytes = Math.max(...traffic.monthly.map((x) => x.bytes), 1);
                      const pct = (m.bytes / maxBytes) * 100;
                      return (
                        <div key={m.year_month} className="flex items-center gap-3">
                          <span className="text-xs text-[#71717a] w-14 flex-shrink-0">{m.year_month}</span>
                          <div className="flex-1 bg-[#27272a] rounded-full h-1.5 overflow-hidden">
                            <div
                              className="h-full bg-[#a78bfa]/60 rounded-full"
                              style={{ width: `${pct}%` }}
                            />
                          </div>
                          <span className="text-xs text-[#a1a1aa] w-16 text-right flex-shrink-0">
                            {fmtBytes(m.bytes)}
                          </span>
                        </div>
                      );
                    })}
                  </div>
                </div>
              )}
            </>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Cluster machines</CardTitle>
          {nodes && (
            <CardDescription>
              {stale
                ? `${nodes.length} machine${nodes.length !== 1 ? "s" : ""} known — status unknown`
                : `${nodes.filter((n) => n.ready).length} of ${nodes.length} ready`}
            </CardDescription>
          )}
        </CardHeader>
        <CardContent>
          {nodes === null ? (
            <p className="text-sm text-[#71717a]">Loading…</p>
          ) : nodes.length === 0 ? (
            <p className="text-sm text-[#71717a]">No machines found.</p>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b border-[#27272a]">
                    <th className="py-2.5 pr-4 text-left text-xs font-medium text-[#71717a]">
                      Name
                    </th>
                    <th className="py-2.5 pr-4 text-left text-xs font-medium text-[#71717a]">
                      Status
                    </th>
                    <th className="py-2.5 pr-4 text-left text-xs font-medium text-[#71717a]">
                      Joined
                    </th>
                    <th className="py-2.5 text-left text-xs font-medium text-[#71717a]">
                      Link
                    </th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-[#27272a]/50">
                  {nodes.map((n) => {
                    const url = urlFor(n.name);
                    return (
                      <tr
                        key={n.name}
                        className="group hover:bg-[#27272a]/20 transition-colors"
                      >
                        <td className="py-3 pr-4 font-medium text-[#fafafa] whitespace-nowrap">
                          {n.name}
                        </td>
                        <td className="py-3 pr-4">
                          {stale ? (
                            <Badge variant="warning">Offline</Badge>
                          ) : (
                            <Badge variant={n.ready ? "success" : "destructive"}>
                              {n.ready ? "Ready" : "Not Ready"}
                            </Badge>
                          )}
                        </td>
                        <td className="py-3 pr-4 text-xs text-[#71717a] whitespace-nowrap">
                          {n.joined_at
                            ? new Date(n.joined_at).toLocaleDateString(
                                undefined,
                                { month: "short", day: "numeric", year: "numeric" },
                              )
                            : "—"}
                        </td>
                        <td className="py-3">
                          {url ? (
                            <a
                              href={url}
                              target="_blank"
                              rel="noreferrer"
                              className="inline-flex items-center gap-1.5 text-sm text-[#a78bfa] hover:text-[#c4b5fd] transition-colors"
                            >
                              {url.replace(/^https?:\/\//, "")}
                              <ExternalLink className="h-3.5 w-3.5 flex-shrink-0" />
                            </a>
                          ) : (
                            <span className="text-xs text-[#3f3f46]">—</span>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

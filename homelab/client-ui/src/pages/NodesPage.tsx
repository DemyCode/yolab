import { useEffect, useState } from "react";
import { ExternalLink, AlertTriangle } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
  CardDescription,
} from "@/components/ui/card";
import type { NodeInfo, NodeLink } from "@/types/nodes";

const CACHE_KEY = "yolab:nodes";

export function NodesPage() {
  const [nodes, setNodes] = useState<NodeInfo[] | null>(null);
  const [links, setLinks] = useState<NodeLink[]>([]);
  const [stale, setStale] = useState(false);

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
  }, []);

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
            Cluster API unreachable — the main node may be restarting. Showing last known state; everything will recover automatically.
          </p>
        </div>
      )}

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

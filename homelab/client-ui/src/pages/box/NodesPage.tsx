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
import { fetchList } from "@/lib/api";

export function NodesPage() {
  const [nodes, setNodes] = useState<NodeInfo[] | null>(null);
  const [links, setLinks] = useState<NodeLink[]>([]);
  const [stale, setStale] = useState(false);

  useEffect(() => {
    void fetchList<NodeInfo>("/api/nodes").then((res) => {
      if (res.ok) {
        setNodes(res.data);
        setStale(false);
      } else {
        // No cache to fall back on any more — an empty list plus the banner
        // below is more honest than either a stale number or an infinite
        // "Loading…" once the request has actually failed.
        setStale(true);
        setNodes((prev) => prev ?? []);
      }
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
      {stale && (
        <div className="flex items-start gap-2.5 rounded-lg border border-warning/30 bg-warning/5 px-4 py-3">
          <AlertTriangle className="h-4 w-4 text-warning mt-0.5 flex-shrink-0" />
          <p className="text-sm text-warning">
            Cluster API unreachable — the control plane is restarting. This will
            update automatically once it responds again.
          </p>
        </div>
      )}

      <Card>
        <CardHeader>
          <CardTitle>Cluster machines</CardTitle>
          {nodes && !stale && (
            <CardDescription>
              {`${nodes.filter((n) => n.ready).length} of ${nodes.length} ready`}
            </CardDescription>
          )}
        </CardHeader>
        <CardContent>
          {nodes === null ? (
            <p className="text-sm text-fg-muted">Loading…</p>
          ) : nodes.length === 0 ? (
            <p className="text-sm text-fg-muted">No machines found.</p>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b border-border">
                    <th className="py-2.5 pr-4 text-left text-xs font-medium text-fg-muted">
                      Name
                    </th>
                    <th className="py-2.5 pr-4 text-left text-xs font-medium text-fg-muted">
                      Status
                    </th>
                    <th className="py-2.5 pr-4 text-left text-xs font-medium text-fg-muted">
                      Joined
                    </th>
                    <th className="py-2.5 text-left text-xs font-medium text-fg-muted">
                      Link
                    </th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-border/50">
                  {nodes.map((n) => {
                    const url = urlFor(n.name);
                    return (
                      <tr
                        key={n.name}
                        className="group hover:bg-border/20 transition-colors"
                      >
                        <td className="py-3 pr-4 font-medium text-fg whitespace-nowrap">
                          {n.name}
                        </td>
                        <td className="py-3 pr-4">
                          {stale ? (
                            <Badge variant="warning">Offline</Badge>
                          ) : (
                            <Badge variant={n.ready ? "success" : "danger"}>
                              {n.ready ? "Ready" : "Not Ready"}
                            </Badge>
                          )}
                        </td>
                        <td className="py-3 pr-4 text-xs text-fg-muted whitespace-nowrap">
                          {n.joined_at
                            ? new Date(n.joined_at).toLocaleDateString(
                                undefined,
                                {
                                  month: "short",
                                  day: "numeric",
                                  year: "numeric",
                                },
                              )
                            : "—"}
                        </td>
                        <td className="py-3">
                          {url ? (
                            <a
                              href={url}
                              target="_blank"
                              rel="noreferrer"
                              className="inline-flex items-center gap-1.5 text-sm text-primary hover:text-primary transition-colors"
                            >
                              {url.replace(/^https?:\/\//, "")}
                              <ExternalLink className="h-3.5 w-3.5 flex-shrink-0" />
                            </a>
                          ) : (
                            <span className="text-xs text-border-strong">
                              —
                            </span>
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

import { useEffect, useState } from "react";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
  CardDescription,
} from "@/components/ui/card";

interface Node {
  name: string;
  ip: string;
  ready: boolean;
  roles: string[];
  joined_at: string;
}

export function NodesPage() {
  const [nodes, setNodes] = useState<Node[] | null>(null);

  useEffect(() => {
    fetch("/api/nodes")
      .then((r) => r.json())
      .then((n) => setNodes(n as Node[]))
      .catch(() => setNodes([]));
  }, []);

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Nodes</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Kubernetes cluster members
        </p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Cluster nodes</CardTitle>
          {nodes && (
            <CardDescription>
              {nodes.filter((n) => n.ready).length} of {nodes.length} ready
            </CardDescription>
          )}
        </CardHeader>
        <CardContent>
          {nodes === null ? (
            <p className="text-sm text-[#71717a]">Loading…</p>
          ) : nodes.length === 0 ? (
            <p className="text-sm text-[#71717a]">No nodes found.</p>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b border-[#27272a]">
                    <th className="py-2.5 pr-4 text-left text-xs font-medium text-[#71717a] whitespace-nowrap">
                      Name
                    </th>
                    <th className="py-2.5 pr-4 text-left text-xs font-medium text-[#71717a] whitespace-nowrap">
                      IP
                    </th>
                    <th className="py-2.5 pr-4 text-left text-xs font-medium text-[#71717a] whitespace-nowrap">
                      Roles
                    </th>
                    <th className="py-2.5 pr-4 text-left text-xs font-medium text-[#71717a] whitespace-nowrap">
                      Status
                    </th>
                    <th className="py-2.5 text-left text-xs font-medium text-[#71717a] whitespace-nowrap">
                      Joined
                    </th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-[#27272a]/50">
                  {nodes.map((n) => (
                    <tr
                      key={n.name}
                      className="group hover:bg-[#27272a]/20 transition-colors"
                    >
                      <td className="py-3 pr-4 font-medium text-[#fafafa] whitespace-nowrap">
                        {n.name}
                      </td>
                      <td className="py-3 pr-4 font-mono text-xs text-[#71717a] whitespace-nowrap">
                        {n.ip}
                      </td>
                      <td className="py-3 pr-4">
                        <div className="flex flex-wrap gap-1">
                          {n.roles.map((r) => (
                            <Badge
                              key={r}
                              variant="muted"
                              className="text-[10px]"
                            >
                              {r}
                            </Badge>
                          ))}
                        </div>
                      </td>
                      <td className="py-3 pr-4">
                        <Badge variant={n.ready ? "success" : "destructive"}>
                          {n.ready ? "Ready" : "Not Ready"}
                        </Badge>
                      </td>
                      <td className="py-3 text-xs text-[#71717a] whitespace-nowrap">
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
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

import { useEffect, useState } from "react";
import { Copy, Eye, EyeOff, Check, ExternalLink } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
  CardDescription,
} from "@/components/ui/card";
import type { NodeInfo, NodeLink, JoinInfo } from "@/types/nodes";

function CopyButton({ value }: { value: string }) {
  const [copied, setCopied] = useState(false);
  function copy() {
    navigator.clipboard.writeText(value).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  }
  return (
    <button
      onClick={copy}
      className="ml-1.5 text-[#52525b] hover:text-[#a1a1aa] transition-colors flex-shrink-0"
      title="Copy"
    >
      {copied ? <Check className="h-3.5 w-3.5 text-[#4ade80]" /> : <Copy className="h-3.5 w-3.5" />}
    </button>
  );
}

function JoinCard({ info }: { info: JoinInfo }) {
  const [showToken, setShowToken] = useState(false);
  const [copiedCmd, setCopiedCmd] = useState(false);

  const command = `k3s agent --server ${info.server_addr} --token ${info.k3s_token}`;

  function copyCommand() {
    navigator.clipboard.writeText(command).then(() => {
      setCopiedCmd(true);
      setTimeout(() => setCopiedCmd(false), 1500);
    });
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Join cluster</CardTitle>
        <CardDescription>
          Run the command below as root on any new machine to add it as a K3s agent node.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {/* Fields */}
        <div className="space-y-2 text-xs font-mono">
          <div className="flex items-center gap-2">
            <span className="text-[#52525b] w-28 flex-shrink-0">Server</span>
            <span className="text-[#a1a1aa] truncate">{info.server_addr}</span>
            <CopyButton value={info.server_addr} />
          </div>
          <div className="flex items-center gap-2">
            <span className="text-[#52525b] w-28 flex-shrink-0">Token</span>
            <span className="text-[#a1a1aa] truncate flex-1 min-w-0">
              {showToken ? info.k3s_token : "••••••••••••••••••••••••••••••••"}
            </span>
            <button
              onClick={() => setShowToken((v) => !v)}
              className="text-[#52525b] hover:text-[#a1a1aa] transition-colors flex-shrink-0"
              title={showToken ? "Hide" : "Reveal"}
            >
              {showToken ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
            </button>
            <CopyButton value={info.k3s_token} />
          </div>
        </div>

        {/* Command block */}
        <div className="rounded-md bg-[#09090b] border border-[#27272a] px-4 py-3">
          <div className="flex items-start justify-between gap-3">
            <code className="text-xs text-[#a1a1aa] font-mono break-all leading-5">
              <span className="text-[#52525b] select-none">$ </span>
              {`k3s agent --server ${info.server_addr} --token `}
              <span className={showToken ? "" : "blur-[3px] select-none"}>
                {info.k3s_token}
              </span>
            </code>
            <Button
              size="sm"
              variant="outline"
              onClick={copyCommand}
              className="flex-shrink-0 h-7 px-2 text-xs border-[#27272a] text-[#a1a1aa] hover:text-[#fafafa]"
            >
              {copiedCmd ? <Check className="h-3.5 w-3.5 text-[#4ade80]" /> : <Copy className="h-3.5 w-3.5" />}
            </Button>
          </div>
        </div>

        <p className="text-xs text-[#52525b]">
          The new node appears in the table above within a few seconds of the agent connecting.
          No reboot required — it joins live.
        </p>
      </CardContent>
    </Card>
  );
}

function NodeLinksCard({ links }: { links: NodeLink[] }) {
  if (links.length === 0) return null;
  return (
    <Card>
      <CardHeader>
        <CardTitle>Management access</CardTitle>
        <CardDescription>
          Each node hosts its own independent management UI — if one goes down, use another.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <div className="space-y-2">
          {links.map((l) => (
            <div key={l.name} className="flex items-center justify-between gap-4 py-1">
              <span className="text-xs font-mono text-[#52525b] w-16 flex-shrink-0">{l.name}</span>
              <a
                href={l.url}
                target="_blank"
                rel="noreferrer"
                className="flex-1 text-sm text-[#a78bfa] hover:text-[#c4b5fd] truncate"
              >
                {l.url.replace(/^https?:\/\//, "")}
              </a>
              <ExternalLink className="h-3.5 w-3.5 text-[#52525b] flex-shrink-0" />
            </div>
          ))}
        </div>
      </CardContent>
    </Card>
  );
}

export function NodesPage() {
  const [nodes, setNodes] = useState<NodeInfo[] | null>(null);
  const [links, setLinks] = useState<NodeLink[]>([]);
  const [joinInfo, setJoinInfo] = useState<JoinInfo | null>(null);

  useEffect(() => {
    fetch("/api/nodes")
      .then((r) => r.json())
      .then((n) => setNodes(n as NodeInfo[]))
      .catch(() => setNodes([]));

    fetch("/api/nodes/links")
      .then((r) => r.json())
      .then((l) => setLinks(l as NodeLink[]))
      .catch(() => {});

    fetch("/api/cluster/join-info")
      .then((r) => r.json())
      .then((j) => setJoinInfo(j as JoinInfo))
      .catch(() => {});
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

      <NodeLinksCard links={links} />
      {joinInfo && <JoinCard info={joinInfo} />}
    </div>
  );
}

import { useEffect, useState } from "react";
import { Database, RefreshCw } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";

// ── Types ─────────────────────────────────────────────────────────────────────

interface S3Info {
  bucket_name: string;
  endpoint: string;
  region: string;
  access_key_id: string;
  created_at: string;
}

interface S3Status {
  provisioned: boolean;
  s3?: S3Info;
}

function Shimmer({ className }: { className?: string }) {
  return (
    <div className={`animate-pulse rounded bg-[#27272a] ${className ?? ""}`} />
  );
}

// ── S3 card ───────────────────────────────────────────────────────────────────

function S3Card({
  status,
  onEnable,
}: {
  status: S3Status | null;
  onEnable: () => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleEnable() {
    setBusy(true);
    setError(null);
    try {
      await onEnable();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed");
    } finally {
      setBusy(false);
    }
  }

  const info = status?.s3;

  return (
    <Card>
      <CardContent className="pt-5 pb-5">
        <div className="flex items-start gap-3">
          <div
            className="mt-0.5 rounded-md p-1.5 flex-shrink-0"
            style={{ background: info ? "#1a2e1a" : "#2d2a1a" }}
          >
            <Database
              className="h-4 w-4"
              style={{ color: info ? "#4ade80" : "#fbbf24" }}
              strokeWidth={1.75}
            />
          </div>

          <div className="flex-1">
            <div className="flex items-center justify-between gap-4 flex-wrap">
              <div>
                <p className="text-sm font-medium text-[#fafafa]">
                  S3 Backup Storage
                </p>
                <p className="text-xs text-[#71717a] mt-0.5">
                  {info
                    ? "Daily Velero backups and etcd snapshots"
                    : "Not provisioned — enable to start storing cluster backups"}
                </p>
              </div>
              {!info && (
                <Button
                  onClick={handleEnable}
                  disabled={busy}
                  className="bg-[#a78bfa] hover:bg-[#9061f9] text-[#09090b] font-medium text-sm h-8 px-3"
                >
                  {busy ? (
                    <>
                      <RefreshCw className="h-3.5 w-3.5 mr-1.5 animate-spin" />
                      Enabling…
                    </>
                  ) : (
                    "Enable Backups"
                  )}
                </Button>
              )}
            </div>

            {info && (
              <div className="mt-3 grid grid-cols-1 gap-1.5 text-xs font-mono">
                <div className="flex gap-2">
                  <span className="text-[#52525b] w-24 flex-shrink-0">Bucket</span>
                  <span className="text-[#a1a1aa] truncate">{info.bucket_name}</span>
                </div>
                <div className="flex gap-2">
                  <span className="text-[#52525b] w-24 flex-shrink-0">Endpoint</span>
                  <span className="text-[#a1a1aa] truncate">{info.endpoint}</span>
                </div>
                <div className="flex gap-2">
                  <span className="text-[#52525b] w-24 flex-shrink-0">Region</span>
                  <span className="text-[#a1a1aa]">{info.region}</span>
                </div>
                <div className="flex gap-2">
                  <span className="text-[#52525b] w-24 flex-shrink-0">Key ID</span>
                  <span className="text-[#a1a1aa] truncate">{info.access_key_id}</span>
                </div>
              </div>
            )}

            {error && (
              <p className="mt-2 text-xs text-[#f87171]">{error}</p>
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ── Page ──────────────────────────────────────────────────────────────────────

export function BackupsPage() {
  const [status, setStatus] = useState<S3Status | null>(null);
  const [loading, setLoading] = useState(true);

  async function load() {
    const res = await fetch("/api/backups/s3")
      .then((r) => r.json())
      .catch(() => ({ provisioned: false }));
    setStatus(res as S3Status);
    setLoading(false);
  }

  useEffect(() => { void load(); }, []);

  async function handleEnable() {
    const res = await fetch("/api/backups/s3/enable", { method: "POST" });
    if (!res.ok) {
      const text = await res.text();
      throw new Error(text || `Server error ${res.status}`);
    }
    await load();
  }

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Backups</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Cluster backups via Velero. Provisioned through your YoLab account.
        </p>
      </div>

      {loading ? (
        <Card>
          <CardContent className="pt-5 pb-5">
            <Shimmer className="h-14 w-full" />
          </CardContent>
        </Card>
      ) : (
        <S3Card status={status} onEnable={handleEnable} />
      )}
    </div>
  );
}

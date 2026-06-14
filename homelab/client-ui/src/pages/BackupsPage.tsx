import { useEffect, useState } from "react";
import { Cloud, Database, HardDrive, RefreshCw } from "lucide-react";
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

interface SftpInfo {
  host: string;
  port: number;
  username: string;
  created_at: string;
}

interface BackupStatus {
  s3: { provisioned: boolean; s3?: S3Info };
  sftp: { provisioned: boolean; sftp?: SftpInfo };
}

function Shimmer({ className }: { className?: string }) {
  return (
    <div className={`animate-pulse rounded bg-[#27272a] ${className ?? ""}`} />
  );
}

// ── S3 backup card ────────────────────────────────────────────────────────────

function S3Card({
  status,
  onEnable,
}: {
  status: BackupStatus["s3"] | null;
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
                    : "Backblaze B2 bucket provisioned per homelab"}
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

// ── SFTP (cloud disk) card ────────────────────────────────────────────────────

function SftpCard({ status }: { status: BackupStatus["sftp"] | null }) {
  const info = status?.sftp;
  return (
    <Card>
      <CardContent className="pt-5 pb-5">
        <div className="flex items-start gap-3">
          <div
            className="mt-0.5 rounded-md p-1.5 flex-shrink-0"
            style={{ background: info ? "#1a2e1a" : "#2d2a1a" }}
          >
            <HardDrive
              className="h-4 w-4"
              style={{ color: info ? "#4ade80" : "#fbbf24" }}
              strokeWidth={1.75}
            />
          </div>

          <div className="flex-1">
            <p className="text-sm font-medium text-[#fafafa]">
              Cloud Disk (SFTP)
            </p>
            <p className="text-xs text-[#71717a] mt-0.5">
              {info
                ? "Hetzner Storage Box mounted as a Ceph OSD"
                : "Add a cloud disk from the Storage page to provision"}
            </p>

            {info && (
              <div className="mt-3 grid grid-cols-1 gap-1.5 text-xs font-mono">
                <div className="flex gap-2">
                  <span className="text-[#52525b] w-24 flex-shrink-0">Host</span>
                  <span className="text-[#a1a1aa]">{info.host}</span>
                </div>
                <div className="flex gap-2">
                  <span className="text-[#52525b] w-24 flex-shrink-0">Port</span>
                  <span className="text-[#a1a1aa]">{info.port}</span>
                </div>
                <div className="flex gap-2">
                  <span className="text-[#52525b] w-24 flex-shrink-0">Username</span>
                  <span className="text-[#a1a1aa]">{info.username}</span>
                </div>
              </div>
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ── Page ──────────────────────────────────────────────────────────────────────

export function BackupsPage() {
  const [status, setStatus] = useState<BackupStatus | null>(null);
  const [loading, setLoading] = useState(true);

  async function load() {
    const [s3Res, sftpRes] = await Promise.all([
      fetch("/api/backups/s3").then((r) => r.json()).catch(() => ({ provisioned: false })),
      fetch("/api/backups/sftp").then((r) => r.json()).catch(() => ({ provisioned: false })),
    ]);
    setStatus({ s3: s3Res as BackupStatus["s3"], sftp: sftpRes as BackupStatus["sftp"] });
    setLoading(false);
  }

  useEffect(() => { void load(); }, []);

  async function handleEnableS3() {
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
        <h1 className="text-xl font-semibold text-[#fafafa]">Backups & Cloud Storage</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Managed storage provisioned through YoLab — one bucket and one SFTP drive per homelab.
        </p>
      </div>

      <div className="flex items-center gap-2 rounded-md bg-[#1a1a2e] border border-[#2d2d5a] px-3 py-2.5">
        <Cloud className="h-4 w-4 text-[#818cf8] flex-shrink-0" strokeWidth={1.75} />
        <p className="text-xs text-[#818cf8]">
          Storage is billed through your YoLab account. Backblaze B2 and Hetzner Storage Box
          are provisioned automatically — you don&apos;t need separate accounts.
        </p>
      </div>

      {loading ? (
        <div className="space-y-3">
          <Card><CardContent className="pt-5 pb-5"><Shimmer className="h-14 w-full" /></CardContent></Card>
          <Card><CardContent className="pt-5 pb-5"><Shimmer className="h-14 w-full" /></CardContent></Card>
        </div>
      ) : (
        <div className="space-y-3">
          <S3Card status={status?.s3 ?? null} onEnable={handleEnableS3} />
          <SftpCard status={status?.sftp ?? null} />
        </div>
      )}
    </div>
  );
}

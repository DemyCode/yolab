import { useState } from "react";
import { AlertTriangle } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Banner } from "@/components/ui/feedback";
import { ConfirmDialog } from "@/components/ui/sheet";
import { api } from "@/lib/api";
import { useApi } from "@/lib/useResource";
import { useRecoveryStatus, type RecoveryPreview } from "@/lib/recovery";

function formatDate(iso: string): string {
  return new Date(iso).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function AppList({ names }: { names: string[] }) {
  if (names.length === 0) return <span className="text-fg-subtle">none</span>;
  return <span className="font-medium text-fg">{names.join(", ")}</span>;
}

function RecoverDialog({
  open,
  onClose,
  onStarted,
}: {
  open: boolean;
  onClose: () => void;
  onStarted: () => void;
}) {
  const preview = useApi<RecoveryPreview>(
    open ? "storage-recovery-preview" : null,
    "/api/storage/recovery/preview",
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function start() {
    setBusy(true);
    setError(null);
    try {
      await api.post("/api/storage/recovery");
      onStarted();
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not start the repair");
    } finally {
      setBusy(false);
    }
  }

  const p = preview.data;
  return (
    <ConfirmDialog
      open={open}
      onClose={onClose}
      onConfirm={() => void start()}
      title="Repair from backup?"
      confirmLabel="Reset storage and repair"
      destructive
      busy={busy || preview.loading}
      body={
        <div className="space-y-3">
          {p && p.down_osds.length > 0 && (
            <p>
              Down right now:{" "}
              <span className="font-medium text-fg">
                {p.down_osds.map((id) => `disk ${id}`).join(", ")}
              </span>
              . If a machine is only restarting or a disk can be plugged back
              in, do that instead — everything comes back on its own.
            </p>
          )}
          <p>
            Storage is reset and every app is reinstalled from your latest
            backup. This cannot be undone: reconnecting the disk afterwards
            brings nothing back.
          </p>
          {preview.loading && <p>Reading your backup…</p>}
          {preview.error && !p && (
            <p className="text-danger">
              Your backup could not be read right now.
            </p>
          )}
          {p && (
            <>
              <p>
                {p.backup_taken_at
                  ? `Latest backup: ${formatDate(p.backup_taken_at)}. Every app goes back to that moment.`
                  : "There is no backup to repair from — no app will come back."}
              </p>
              <p>
                Come back: <AppList names={p.restored} />
              </p>
              <p>
                Do not come back (not in the backup):{" "}
                <AppList names={p.not_restored} />
              </p>
            </>
          )}
          {error && <p className="text-danger">{error}</p>}
        </div>
      }
    />
  );
}

/** The button and its confirmation, wherever the loss is shown. */
function RepairButton({ size }: { size?: "sm" }) {
  const status = useRecoveryStatus();
  const [confirming, setConfirming] = useState(false);
  return (
    <>
      <Button variant="danger" size={size} onClick={() => setConfirming(true)}>
        Repair from backup
      </Button>
      <RecoverDialog
        open={confirming}
        onClose={() => setConfirming(false)}
        onStarted={() => void status.refresh()}
      />
    </>
  );
}

function lossTitle(groups: number): string {
  return `${groups} group${groups === 1 ? "" : "s"} of your files ${groups === 1 ? "is" : "are"} unavailable`;
}

/** The Backups page section while app data is unavailable. */
export function StorageRecoveryCard() {
  const loss = useRecoveryStatus().data?.loss;
  if (!loss?.needs_recovery) return null;
  return (
    <Card className="border-danger/30 bg-danger-soft">
      <CardContent className="space-y-3 pt-5 pb-5">
        <p className="flex items-center gap-2 text-sm font-medium text-danger">
          <AlertTriangle className="h-4 w-4" />
          {lossTitle(loss.placement_groups)}
        </p>
        <p className="text-sm text-fg-muted">
          Every disk holding them is down. If you can, reconnect the disk or
          restart the machine: everything comes back on its own. Otherwise,
          repair from your latest backup. Backups are paused until then, so
          nothing damaged replaces a good backup.
        </p>
        <RepairButton />
      </CardContent>
    </Card>
  );
}

/** The home page banner while app data is unavailable, with the button right in it. */
export function StorageRecoveryBanner({ className }: { className?: string }) {
  const loss = useRecoveryStatus().data?.loss;
  if (!loss?.needs_recovery) return null;
  return (
    <Banner
      tone="error"
      title={lossTitle(loss.placement_groups)}
      className={className}
      action={<RepairButton size="sm" />}
    >
      Reconnect the disk or restart the machine to get everything back — or
      repair from your latest backup.
    </Banner>
  );
}

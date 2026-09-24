import { useState } from "react";
import { CheckCircle, Copy, KeyRound } from "lucide-react";
import { Button } from "@/components/ui/button";

export function RecoveryKeyOverlay({
  recoveryKey,
  mandatory,
  onClose,
}: {
  recoveryKey: string;
  mandatory: boolean;
  onClose: () => void;
}) {
  const [acknowledged, setAcknowledged] = useState(false);
  const [copied, setCopied] = useState(false);

  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(recoveryKey);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
      // eslint-disable-next-line no-empty
    } catch {}
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-bg/95 p-6 backdrop-blur-sm">
      <div className="w-full max-w-lg space-y-4 rounded-lg border border-border-strong bg-surface p-6">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 shrink-0 rounded-md bg-warning-soft p-1.5">
            <KeyRound className="h-4 w-4 text-warning" strokeWidth={1.75} />
          </div>
          <div>
            <p className="text-sm font-semibold text-fg">
              Your backup recovery key
            </p>
            <p className="mt-1 text-xs text-fg-muted">
              This is the only way to read your backups if this machine is lost
              or destroyed. YoLab keeps no copy anywhere else. Save it now in a
              password manager or print it — without it, your backups are
              permanently unreadable.
            </p>
          </div>
        </div>

        <div className="flex items-center gap-2">
          <code className="flex-1 select-all break-all rounded border border-border-strong bg-surface-2 px-3 py-2 font-mono text-sm text-fg">
            {recoveryKey}
          </code>
          <Button
            onClick={() => void handleCopy()}
            variant="outline"
            className="h-9 shrink-0 border-border-strong px-3 text-xs text-fg-muted hover:text-fg"
          >
            {copied ? (
              <CheckCircle className="h-3.5 w-3.5 text-success" />
            ) : (
              <Copy className="h-3.5 w-3.5" />
            )}
          </Button>
        </div>

        {mandatory && (
          <label className="flex cursor-pointer items-start gap-2">
            <input
              type="checkbox"
              checked={acknowledged}
              onChange={() => setAcknowledged((a) => !a)}
              className="mt-0.5 h-4 w-4 rounded border-border-strong bg-surface-2 accent-primary"
            />
            <span className="text-xs text-fg-muted">
              I have saved this recovery key somewhere safe and durable.
            </span>
          </label>
        )}

        <div className="flex justify-end">
          <Button
            onClick={onClose}
            disabled={mandatory && !acknowledged}
            size="sm"
          >
            {mandatory ? "I have saved it — continue" : "Close"}
          </Button>
        </div>
      </div>
    </div>
  );
}

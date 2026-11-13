import { useEffect, useState } from "react";
import { api } from "@/lib/api";
import { RecoveryKeyOverlay } from "@/components/RecoveryKeyOverlay";

type RecoveryKeyStatus = {
  configured: boolean;
  recovery_key?: string;
  acknowledged?: boolean;
};

export function RecoveryKeyGate() {
  const [key, setKey] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .get<RecoveryKeyStatus>("/api/backups/recovery-key")
      .then((data) => {
        if (cancelled) return;
        if (data.configured && data.recovery_key && !data.acknowledged) {
          setKey(data.recovery_key);
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  if (!key) return null;

  return (
    <RecoveryKeyOverlay
      recoveryKey={key}
      mandatory
      onAcknowledge={() => api.post("/api/backups/recovery-key/ack")}
      onClose={() => setKey(null)}
    />
  );
}

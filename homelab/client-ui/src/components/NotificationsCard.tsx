import { useMemo, useState } from "react";
import { Bell } from "lucide-react";
import qrcode from "qrcode-generator";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { api } from "@/lib/api";
import { useApi } from "@/lib/useResource";

/** `GET /api/notifications` — see local-api notify/mod.rs. */
type NotificationsStatus =
  | {
      available: true;
      subscription: {
        topic: string;
        subscribe_url: string;
        web_url: string;
      };
    }
  | { available: false; reason: string };

/** A QR code as one SVG path: one square per dark module. */
function QrCode({ value, size = 184 }: { value: string; size?: number }) {
  const { path, count } = useMemo(() => {
    const qr = qrcode(0, "M");
    qr.addData(value);
    qr.make();
    const n = qr.getModuleCount();
    let d = "";
    for (let row = 0; row < n; row++) {
      for (let col = 0; col < n; col++) {
        if (qr.isDark(row, col)) d += `M${col} ${row}h1v1h-1z`;
      }
    }
    return { path: d, count: n };
  }, [value]);
  // A quiet zone of 4 modules, which scanners need to find the code.
  const quiet = 4;
  return (
    <svg
      width={size}
      height={size}
      viewBox={`${-quiet} ${-quiet} ${count + quiet * 2} ${count + quiet * 2}`}
      role="img"
      aria-label="QR code to subscribe to notifications"
      className="rounded-md bg-white"
      shapeRendering="crispEdges"
    >
      <rect
        x={-quiet}
        y={-quiet}
        width={count + quiet * 2}
        height={count + quiet * 2}
        fill="#fff"
      />
      <path d={path} fill="#000" />
    </svg>
  );
}

/** The System page section: subscribe a phone to the cluster's notifications. */
export function NotificationsCard() {
  const status = useApi<NotificationsStatus>(
    "notifications",
    "/api/notifications",
  );
  const [sending, setSending] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  const s = status.data;

  async function sendTest() {
    setSending(true);
    setResult(null);
    try {
      await api.post("/api/notifications/test");
      setResult("Sent. It should arrive on every subscribed phone.");
    } catch (e) {
      setResult(e instanceof Error ? e.message : "Could not send it");
    } finally {
      setSending(false);
    }
  }

  return (
    <Card>
      <CardContent className="space-y-4 pt-5 pb-5">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md bg-primary/10 p-1.5">
            <Bell className="h-4 w-4 text-primary" strokeWidth={1.75} />
          </div>
          <div className="min-w-0">
            <p className="text-sm font-medium text-fg">Phone notifications</p>
            <p className="mt-0.5 text-sm text-fg-muted">
              Get told on your phone when a machine stops answering, files lose
              their copy, a backup fails or a disk cannot be added.
            </p>
          </div>
        </div>

        {!s ? null : !s.available ? (
          <p className="text-sm text-fg-muted">{s.reason}</p>
        ) : (
          <div className="flex flex-col gap-4 sm:flex-row sm:items-start">
            <QrCode value={s.subscription.subscribe_url} />
            <div className="min-w-0 space-y-3 text-sm text-fg-muted">
              <ol className="list-decimal space-y-1 pl-5">
                <li>
                  Install the <span className="text-fg">ntfy</span> app on your
                  phone.
                </li>
                <li>
                  Scan this code, or open{" "}
                  <a
                    className="break-all text-primary underline"
                    href={s.subscription.subscribe_url}
                  >
                    the link
                  </a>{" "}
                  on the phone.
                </li>
                <li>
                  That is all: every machine sends to the same address, so a
                  machine that is down does not silence the others.
                </li>
              </ol>
              <p>
                In a browser:{" "}
                <a
                  className="break-all text-primary underline"
                  href={s.subscription.web_url}
                  target="_blank"
                  rel="noreferrer"
                >
                  {s.subscription.web_url}
                </a>
              </p>
              <p className="text-xs text-fg-subtle">
                Anyone with this code can read these notifications. Share it
                only with people who look after this machine.
              </p>
              <div className="flex flex-wrap items-center gap-3">
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => void sendTest()}
                  loading={sending}
                >
                  Send a test notification
                </Button>
                {result && <span className="text-xs">{result}</span>}
              </div>
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

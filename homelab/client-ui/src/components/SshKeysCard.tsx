import { useState } from "react";
import { KeyRound, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { api } from "@/lib/api";
import { useApi } from "@/lib/useResource";
import {
  keyName,
  looksLikePrivateKey,
  sshCommand,
  type SshKeys,
} from "@/lib/ssh";

export function SshKeysCard() {
  const res = useApi<SshKeys>("ssh-keys", "/api/ssh/keys", { persist: false });
  const [key, setKey] = useState("");
  const [label, setLabel] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const data = res.data;
  const privateKey = looksLikePrivateKey(key);

  async function add() {
    setBusy("add");
    setError(null);
    try {
      const next = await api.post<SshKeys>("/api/ssh/keys", { key, label });
      res.mutate(next);
      setKey("");
      setLabel("");
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not add the key");
    } finally {
      setBusy(null);
    }
  }

  async function remove(id: string) {
    setBusy(id);
    setError(null);
    try {
      const next = await api.post<SshKeys>("/api/ssh/keys/remove", { id });
      res.mutate(next);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove the key");
    } finally {
      setBusy(null);
    }
  }

  return (
    <Card>
      <CardContent className="space-y-4 pt-5 pb-5">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-control bg-surface-2 p-1.5">
            <KeyRound className="h-4 w-4 text-fg-muted" strokeWidth={1.75} />
          </div>
          <div className="min-w-0">
            <p className="text-sm font-medium text-fg">SSH access</p>
            <p className="mt-0.5 text-sm text-fg-muted">
              Add the public key of a computer you trust to log in to every
              machine from a terminal. Only keys work: there is no password
              login.
            </p>
          </div>
        </div>

        {res.error && !data && (
          <p className="text-sm text-danger">{res.error}</p>
        )}

        {data && data.keys.length > 0 && (
          <div className="space-y-2">
            <ul className="divide-y divide-border rounded-control border border-border">
              {data.keys.map((k) => (
                <li
                  key={k.id}
                  className="flex items-center justify-between gap-3 px-3 py-2.5"
                >
                  <div className="min-w-0">
                    <p className="truncate text-sm text-fg">{keyName(k)}</p>
                    <p className="truncate font-mono text-xs text-fg-subtle">
                      {k.fingerprint}
                    </p>
                  </div>
                  <Button
                    variant="quiet"
                    size="icon-sm"
                    aria-label={`Remove ${keyName(k)}`}
                    loading={busy === k.id}
                    disabled={busy !== null}
                    onClick={() => void remove(k.id)}
                  >
                    <Trash2 className="h-4 w-4" />
                  </Button>
                </li>
              ))}
            </ul>
            <p className="text-sm text-fg-muted">
              Connect with{" "}
              <code className="break-all rounded bg-surface-2 px-1.5 py-0.5 font-mono text-xs text-fg">
                {sshCommand(data.user, window.location.hostname, data.port)}
              </code>
              . Use <code className="font-mono text-xs">sudo</code> with your
              YoLab password for admin commands.
            </p>
          </div>
        )}

        <div className="space-y-2">
          <textarea
            value={key}
            onChange={(e) => setKey(e.target.value)}
            rows={3}
            spellCheck={false}
            placeholder="ssh-ed25519 AAAA… you@laptop"
            aria-label="Public key"
            className="w-full rounded-control border border-border-strong bg-surface px-3 py-2 font-mono text-sm text-fg placeholder:text-fg-subtle focus:border-primary focus:outline-none focus:ring-2 focus:ring-primary/20"
          />
          {privateKey && (
            <p className="text-sm text-danger">
              This is a private key. Keep it secret and never paste it
              anywhere: paste the file that ends in .pub instead.
            </p>
          )}
          <div className="flex flex-col gap-2 sm:flex-row">
            <Input
              value={label}
              onChange={(e) => setLabel(e.target.value)}
              placeholder="Name, e.g. Work laptop (optional)"
              aria-label="Key name"
              maxLength={60}
            />
            <Button
              onClick={() => void add()}
              loading={busy === "add"}
              disabled={!key.trim() || privateKey || busy !== null}
            >
              Add key
            </Button>
          </div>
          <p className="text-xs text-fg-subtle">
            On your computer, run{" "}
            <code className="font-mono">cat ~/.ssh/id_ed25519.pub</code> and
            paste what it prints. No key yet? Run{" "}
            <code className="font-mono">ssh-keygen -t ed25519</code> first.
          </p>
          {error && <p className="text-sm text-danger">{error}</p>}
        </div>
      </CardContent>
    </Card>
  );
}

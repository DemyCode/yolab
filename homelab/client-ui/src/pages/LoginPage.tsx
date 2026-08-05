import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Field, Input } from "@/components/ui/input";
import { Logo } from "@/components/Logo";
import { api, ApiError } from "@/lib/api";

export function LoginPage({ onLogin }: { onLogin: () => void }) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setLoading(true);
    setError("");
    try {
      await api.post("/api/login", { password });
      onLogin();
    } catch (err) {
      setError(
        err instanceof ApiError && err.status === 401
          ? "That password does not match."
          : err instanceof Error
            ? err.message
            : "Could not sign in.",
      );
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="relative flex min-h-screen items-center justify-center overflow-hidden bg-bg p-5">
      {/* Light coming in through a window: the one moment in the product with
          room for atmosphere, and the first thing anyone ever sees of it. */}
      <div
        aria-hidden
        className="pointer-events-none absolute -top-40 left-1/2 h-[34rem] w-[34rem] -translate-x-1/2 rounded-full bg-primary/10 blur-3xl"
      />

      <div className="relative w-full max-w-sm animate-rise-in">
        <div className="mb-8 flex flex-col items-center gap-4 text-center">
          <Logo className="h-12 w-12" />
          <div>
            <h1 className="font-display text-3xl text-fg">Welcome home</h1>
            <p className="mt-1.5 text-sm text-fg-muted">
              Sign in to your server.
            </p>
          </div>
        </div>

        <form
          onSubmit={submit}
          className="space-y-4 rounded-card border border-border bg-surface p-6 shadow-[var(--shadow-lift)]"
        >
          <Field label="Password" error={error || null} htmlFor="password">
            <Input
              id="password"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="••••••••••••"
              autoFocus
              autoComplete="current-password"
            />
          </Field>

          <Button type="submit" full loading={loading} disabled={!password}>
            Sign in
          </Button>
        </form>
      </div>
    </div>
  );
}

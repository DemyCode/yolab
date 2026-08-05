import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Field, Input } from "@/components/ui/input";
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
    <div className="flex min-h-screen items-center justify-center bg-bg p-5">
      <div className="w-full max-w-sm">
        <div className="mb-8 flex flex-col items-center gap-2 text-center">
          <span className="text-4xl" aria-hidden>
            🏡
          </span>
          <h1 className="text-xl font-semibold tracking-tight text-fg">
            Welcome back
          </h1>
          <p className="text-sm text-fg-muted">Sign in to the box at home.</p>
        </div>

        <form
          onSubmit={submit}
          className="space-y-4 rounded-card border border-border bg-surface p-6 shadow-[var(--shadow-card)]"
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

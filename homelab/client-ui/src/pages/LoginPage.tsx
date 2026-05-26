import { useState } from "react";
import { Boxes } from "lucide-react";
import { Button } from "@/components/ui/button";

export function LoginPage({ onLogin }: { onLogin: () => void }) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setLoading(true);
    setError("");
    const r = await fetch("/api/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ password }),
    });
    setLoading(false);
    if (r.ok) {
      onLogin();
    } else {
      const d = await r.json().catch(() => ({}));
      setError((d as { detail?: string }).detail ?? "Invalid password");
    }
  }

  return (
    <div className="min-h-screen bg-[#09090b] flex items-center justify-center p-4">
      <div className="w-full max-w-sm">
        {/* Logo */}
        <div className="flex items-center gap-2.5 justify-center mb-8">
          <Boxes className="h-6 w-6 text-[#a78bfa]" strokeWidth={1.5} />
          <span className="text-lg font-semibold text-[#fafafa]">YoLab</span>
        </div>

        {/* Card */}
        <div className="rounded-2xl border border-[#27272a] bg-[#111114] p-8">
          <div className="mb-6">
            <h1 className="text-base font-semibold text-[#fafafa]">
              Welcome back
            </h1>
            <p className="text-sm text-[#71717a] mt-1">
              Enter your homelab password to continue
            </p>
          </div>

          <form onSubmit={submit} className="space-y-4">
            <div>
              <label className="block text-xs font-medium text-[#a1a1aa] mb-1.5">
                Password
              </label>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="••••••••••••"
                autoFocus
                className="w-full rounded-md border border-[#27272a] bg-[#09090b] px-3 py-2 text-sm text-[#fafafa] placeholder-[#3f3f46] outline-none transition-colors focus:border-[#a78bfa] focus:ring-2 focus:ring-[#a78bfa]/15"
              />
            </div>

            {error && <p className="text-xs text-[#f87171]">{error}</p>}

            <Button
              type="submit"
              disabled={loading || !password}
              className="w-full"
            >
              {loading ? "Signing in…" : "Sign in"}
            </Button>
          </form>
        </div>
      </div>
    </div>
  );
}

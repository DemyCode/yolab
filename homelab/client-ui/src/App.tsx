import { useEffect, useState } from "react";
import { AppsPage } from "./AppsPage";
import { DisksPage } from "./DisksPage";
import { NodesPage } from "./NodesPage";
import { OverviewPage } from "./OverviewPage";
import { TerminalPage } from "./TerminalPage";

function LoginPage({ onLogin }: { onLogin: () => void }) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setLoading(true); setError("");
    const r = await fetch("/api/login", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ password }) });
    setLoading(false);
    if (r.ok) onLogin();
    else { const d = await r.json().catch(() => ({})); setError(d.detail ?? "Login failed"); }
  }

  return (
    <div style={{ position: "fixed", inset: 0, display: "flex", alignItems: "center", justifyContent: "center", background: "#f9fafb" }}>
      <form onSubmit={submit} style={{ background: "#fff", border: "1px solid #e5e7eb", borderRadius: 10, padding: "2rem 2.5rem", display: "flex", flexDirection: "column", gap: "1rem", minWidth: 300, fontFamily: "monospace" }}>
        <h2 style={{ margin: 0, fontSize: "1.2rem" }}>YoLab</h2>
        <p style={{ margin: 0, color: "#666", fontSize: "0.85rem" }}>Enter your homelab password to continue.</p>
        <input type="password" value={password} onChange={e => setPassword(e.target.value)} placeholder="Password" autoFocus
          style={{ padding: "0.5rem 0.75rem", fontSize: "0.95rem", border: "1px solid #d1d5db", borderRadius: 6, fontFamily: "monospace", outline: "none" }} />
        {error && <div style={{ color: "#ef4444", fontSize: "0.8rem" }}>{error}</div>}
        <button type="submit" disabled={loading || !password}
          style={{ padding: "0.55rem 1.2rem", background: loading ? "#999" : "#1a1a1a", color: "#fff", border: "none", borderRadius: 6, fontSize: "0.95rem", cursor: loading ? "not-allowed" : "pointer", fontFamily: "monospace" }}>
          {loading ? "…" : "Login"}
        </button>
      </form>
    </div>
  );
}

function useRouter() {
  const [path, setPath] = useState(() => window.location.hash.slice(1) || "/overview");
  useEffect(() => {
    const handler = () => setPath(window.location.hash.slice(1) || "/overview");
    window.addEventListener("hashchange", handler);
    return () => window.removeEventListener("hashchange", handler);
  }, []);
  function navigate(to: string) { window.location.hash = to; }
  return { path, navigate };
}

function activeSection(path: string): string {
  if (path.startsWith("/nodes")) return "nodes";
  if (path.startsWith("/disks")) return "disks";
  if (path.startsWith("/apps") || path.startsWith("/installed")) return "apps";
  if (path.startsWith("/terminal")) return "terminal";
  return "overview";
}

const TABS = [
  { id: "overview", label: "Overview", href: "/overview" },
  { id: "nodes",    label: "Nodes",    href: "/nodes" },
  { id: "disks",    label: "Disks",    href: "/disks" },
  { id: "apps",     label: "Apps",     href: "/apps" },
  { id: "terminal", label: "Terminal", href: "/terminal" },
];

function App() {
  const { path, navigate } = useRouter();
  const [loggedIn, setLoggedIn] = useState<boolean | null>(null);

  useEffect(() => {
    fetch("/api/status").then(r => setLoggedIn(r.status !== 401)).catch(() => setLoggedIn(true));
  }, []);

  async function logout() {
    await fetch("/api/logout", { method: "POST" });
    setLoggedIn(false);
  }

  if (loggedIn === null) return null;
  if (!loggedIn) return <LoginPage onLogin={() => setLoggedIn(true)} />;

  const section = activeSection(path);

  return (
    <div style={{ fontFamily: "monospace", maxWidth: 960, margin: "3rem auto", padding: "0 1rem" }}>
      <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: "0.25rem" }}>
        <h1 style={{ fontSize: "1.6rem", margin: 0, cursor: "pointer" }} onClick={() => navigate("/overview")}>YoLab</h1>
        <button onClick={logout} style={{ background: "none", border: "none", cursor: "pointer", fontSize: "0.8rem", color: "#999", fontFamily: "monospace" }}>Logout</button>
      </div>
      <p style={{ color: "#666", marginTop: 0, marginBottom: "1rem" }}>Your homelab is up and running.</p>

      <div style={{ display: "flex", gap: "0.25rem", marginBottom: "1.5rem", borderBottom: "2px solid #e5e7eb" }}>
        {TABS.map(({ id, label, href }) => (
          <button key={id} onClick={() => navigate(href)} style={{
            background: "none", border: "none",
            borderBottom: section === id ? "2px solid #1a1a1a" : "2px solid transparent",
            marginBottom: -2, padding: "0.5rem 0.75rem",
            cursor: "pointer", fontFamily: "monospace", fontSize: "0.9rem",
            fontWeight: section === id ? "bold" : "normal",
          }}>{label}</button>
        ))}
      </div>

      {section === "overview" && <OverviewPage />}
      {section === "nodes"    && <NodesPage />}
      {section === "disks"    && <DisksPage />}
      {section === "apps"     && <AppsPage path={path} navigate={navigate} />}
      {section === "terminal" && <TerminalPage />}
    </div>
  );
}

export default App;

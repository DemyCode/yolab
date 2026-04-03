import Form from "@rjsf/core";
import validator from "@rjsf/validator-ajv8";
import { useEffect, useRef, useState } from "react";

interface CatalogApp {
  id: string;
  name: string;
  description: string;
  icon: string;
  category: string;
  requires_tunnel: boolean;
  default_subdomain: string;
  schema: object;
  uischema: object;
}

interface InstalledApp {
  app_id: string;
  instance_name: string;
  subdomain: string;
  domain: string;
  tunnel_url: string;
  storage_size: string;
}

function InstalledCard({ app }: { app: InstalledApp }) {
  return (
    <div style={{
      border: "1px solid #e5e7eb",
      borderRadius: 8,
      padding: "1rem 1.25rem",
      display: "flex",
      justifyContent: "space-between",
      alignItems: "center",
    }}>
      <div>
        <div style={{ fontWeight: "bold" }}>{app.app_id}</div>
        <div style={{ fontSize: "0.8rem", color: "#666", marginTop: 2 }}>{app.instance_name}</div>
      </div>
      <a
        href={app.tunnel_url}
        target="_blank"
        rel="noreferrer"
        style={{ fontSize: "0.82rem", color: "#1a1a1a", fontFamily: "monospace" }}
      >
        {app.tunnel_url}
      </a>
    </div>
  );
}

function InstallModal({
  app,
  onClose,
}: {
  app: CatalogApp;
  onClose: () => void;
}) {
  const [instanceName, setInstanceName] = useState(app.default_subdomain);
  const [subdomain, setSubdomain] = useState(app.default_subdomain);
  const [storageSize, setStorageSize] = useState("50Gi");
  const [formData, setFormData] = useState<object>({});
  const [log, setLog] = useState<string[]>([]);
  const [installing, setInstalling] = useState(false);
  const [done, setDone] = useState(false);
  const [error, setError] = useState("");
  const [tunnelDomain, setTunnelDomain] = useState("");
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (app.requires_tunnel) {
      fetch("/api/tunnel/domain").then((r) => r.json()).then((d) => setTunnelDomain(d.domain)).catch(() => {});
    }
  }, [app.requires_tunnel]);

  useEffect(() => {
    if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight;
  }, [log]);

  async function install() {
    setInstalling(true);
    setLog([]);
    setError("");

    const response = await fetch(`/api/apps/${app.id}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        instance_name: instanceName,
        subdomain,
        storage_size: storageSize,
        config: formData,
      }),
    });

    if (!response.body) { setInstalling(false); return; }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";

    while (true) {
      const { done: streamDone, value } = await reader.read();
      if (streamDone) break;
      buf += decoder.decode(value, { stream: true });
      const parts = buf.split("\n\n");
      buf = parts.pop() ?? "";
      for (const part of parts) {
        const line = part.startsWith("data: ") ? part.slice(6) : part;
        if (!line.trim()) continue;
        if (line.startsWith("[ERROR]")) setError(line);
        if (line.startsWith("[DONE]")) setDone(true);
        setLog((l) => [...l, line]);
      }
    }
    setInstalling(false);
  }

  return (
    <div style={{
      position: "fixed", inset: 0, background: "rgba(0,0,0,0.4)",
      display: "flex", alignItems: "center", justifyContent: "center", zIndex: 100,
    }}>
      <div style={{
        background: "#fff", borderRadius: 12, padding: "2rem",
        width: "100%", maxWidth: 560, maxHeight: "90vh", overflowY: "auto",
      }}>
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: "1.5rem" }}>
          <h2 style={{ margin: 0, fontSize: "1.2rem" }}>{app.icon} Install {app.name}</h2>
          <button onClick={onClose} style={{ background: "none", border: "none", cursor: "pointer", fontSize: "1.2rem" }}>✕</button>
        </div>

        <div style={{ marginBottom: "1rem" }}>
          <label style={{ display: "block", fontSize: "0.78rem", fontWeight: "bold", marginBottom: 4 }}>Instance name</label>
          <input
            value={instanceName}
            onChange={(e) => setInstanceName(e.target.value)}
            style={{ width: "100%", border: "1px solid #e5e7eb", borderRadius: 6, padding: "0.4rem 0.6rem", fontSize: "0.9rem", boxSizing: "border-box" }}
          />
        </div>

        <div style={{ marginBottom: "1rem" }}>
          <label style={{ display: "block", fontSize: "0.78rem", fontWeight: "bold", marginBottom: 4 }}>Tunnel subdomain</label>
          <input
            value={subdomain}
            onChange={(e) => setSubdomain(e.target.value)}
            style={{ width: "100%", border: "1px solid #e5e7eb", borderRadius: 6, padding: "0.4rem 0.6rem", fontSize: "0.9rem", boxSizing: "border-box" }}
          />
          {app.requires_tunnel && (
            <div style={{ fontSize: "0.75rem", color: "#7c3aed", marginTop: 4, fontFamily: "monospace" }}>
              {tunnelDomain ? `https://${subdomain}.${tunnelDomain}` : "A new yolab tunnel will be created for this app."}
            </div>
          )}
        </div>

        <div style={{ marginBottom: "1.5rem" }}>
          <label style={{ display: "block", fontSize: "0.78rem", fontWeight: "bold", marginBottom: 4 }}>Storage size</label>
          <input
            value={storageSize}
            onChange={(e) => setStorageSize(e.target.value)}
            style={{ width: "100%", border: "1px solid #e5e7eb", borderRadius: 6, padding: "0.4rem 0.6rem", fontSize: "0.9rem", boxSizing: "border-box" }}
          />
        </div>

        <Form
          schema={app.schema as never}
          uiSchema={app.uischema as never}
          validator={validator}
          formData={formData}
          onChange={({ formData: d }) => setFormData(d ?? {})}
          onSubmit={() => install()}
        >
          <button
            type="submit"
            disabled={installing || done}
            style={{
              width: "100%",
              padding: "0.65rem",
              background: done ? "#22c55e" : installing ? "#999" : "#1a1a1a",
              color: "#fff",
              border: "none",
              borderRadius: 6,
              cursor: installing || done ? "not-allowed" : "pointer",
              fontWeight: "bold",
              marginTop: "0.5rem",
            }}
          >
            {done ? "Installed!" : installing ? "Installing…" : "Install"}
          </button>
        </Form>

        {log.length > 0 && (
          <div
            ref={logRef}
            style={{
              marginTop: "1rem",
              background: "#111",
              color: "#d1fae5",
              borderRadius: 6,
              padding: "0.75rem",
              fontSize: "0.75rem",
              maxHeight: 200,
              overflowY: "auto",
              fontFamily: "monospace",
              whiteSpace: "pre-wrap",
            }}
          >
            {log.map((l, i) => (
              <div key={i} style={{ color: l.startsWith("[ERROR]") ? "#f87171" : "#d1fae5" }}>{l}</div>
            ))}
          </div>
        )}
        {error && <div style={{ marginTop: "0.5rem", color: "#ef4444", fontSize: "0.82rem" }}>{error}</div>}
      </div>
    </div>
  );
}

export function AppsPage() {
  const [catalog, setCatalog] = useState<CatalogApp[]>([]);
  const [installed, setInstalled] = useState<InstalledApp[]>([]);
  const [installing, setInstalling] = useState<CatalogApp | null>(null);

  useEffect(() => {
    fetch("/api/apps/catalog").then((r) => r.json()).then(setCatalog).catch(() => {});
    fetch("/api/apps").then((r) => r.json()).then(setInstalled).catch(() => {});
  }, []);

  return (
    <div>
      {installed.length > 0 && (
        <div style={{ marginBottom: "2rem" }}>
          <h3 style={{ fontSize: "0.9rem", color: "#666", marginBottom: "0.75rem", marginTop: 0 }}>Installed</h3>
          <div style={{ display: "flex", flexDirection: "column", gap: "0.5rem" }}>
            {installed.map((a) => <InstalledCard key={a.instance_name} app={a} />)}
          </div>
        </div>
      )}

      <h3 style={{ fontSize: "0.9rem", color: "#666", marginBottom: "0.75rem", marginTop: 0 }}>Available</h3>
      <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fill, minmax(220px, 1fr))", gap: "1rem" }}>
        {catalog.map((app) => (
          <div
            key={app.id}
            style={{ border: "1px solid #e5e7eb", borderRadius: 8, padding: "1.25rem", cursor: "pointer" }}
            onClick={() => setInstalling(app)}
          >
            <div style={{ fontSize: "2rem", marginBottom: "0.5rem" }}>{app.icon}</div>
            <div style={{ fontWeight: "bold", marginBottom: "0.25rem" }}>{app.name}</div>
            <div style={{ fontSize: "0.8rem", color: "#666" }}>{app.description}</div>
            {app.requires_tunnel && (
              <div style={{ marginTop: "0.5rem", fontSize: "0.72rem", color: "#7c3aed" }}>Tunnel required</div>
            )}
          </div>
        ))}
      </div>

      {installing && (
        <InstallModal
          app={installing}
          onClose={() => {
            setInstalling(null);
            fetch("/api/apps").then((r) => r.json()).then(setInstalled).catch(() => {});
          }}
        />
      )}
    </div>
  );
}

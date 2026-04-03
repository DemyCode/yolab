import Form from "@rjsf/core";
import type { WidgetProps } from "@rjsf/utils";
import validator from "@rjsf/validator-ajv8";
import { useEffect, useRef, useState } from "react";

interface CatalogApp {
  id: string;
  name: string;
  description: string;
  icon: string;
  category: string;
  schema: object;
  uischema: object;
}

interface InstalledApp {
  app_id: string;
  instance_name: string;
  tunnel_url: string;
}

function TunnelWidget({ value, onChange, registry }: WidgetProps) {
  const { tunnelDomain } = registry.formContext as { tunnelDomain: string };
  return (
    <div>
      <input
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value)}
        style={{ width: "100%", border: "1px solid #e5e7eb", borderRadius: 6, padding: "0.4rem 0.6rem", fontSize: "0.9rem", boxSizing: "border-box" }}
      />
      {tunnelDomain && value && (
        <div style={{ fontSize: "0.75rem", color: "#7c3aed", marginTop: 4, fontFamily: "monospace" }}>
          {`https://${value}.${tunnelDomain}`}
        </div>
      )}
    </div>
  );
}

interface Pod {
  name: string;
  phase: string;
  ready: boolean;
}

function phaseColor(pod: Pod) {
  if (pod.ready) return "#22c55e";
  if (pod.phase === "Pending" || pod.phase === "Running") return "#f59e0b";
  return "#ef4444";
}

function AppDetailModal({ app, onClose }: { app: InstalledApp; onClose: () => void }) {
  const [pods, setPods] = useState<Pod[]>([]);
  const [selectedPod, setSelectedPod] = useState<string | null>(null);
  const [view, setView] = useState<"describe" | "logs">("describe");
  const [describe, setDescribe] = useState<string>("");
  const [logs, setLogs] = useState<string[]>([]);
  const outputRef = useRef<HTMLDivElement>(null);
  const readerRef = useRef<ReadableStreamDefaultReader | null>(null);

  useEffect(() => {
    fetchPods();
    const id = setInterval(fetchPods, 3000);
    return () => clearInterval(id);
  }, []);

  useEffect(() => {
    if (outputRef.current) outputRef.current.scrollTop = outputRef.current.scrollHeight;
  }, [logs, describe]);

  async function fetchPods() {
    const r = await fetch(`/api/apps/${app.instance_name}/pods`).catch(() => null);
    if (r?.ok) setPods(await r.json());
  }

  async function selectPod(podName: string) {
    if (readerRef.current) { readerRef.current.cancel(); readerRef.current = null; }
    setSelectedPod(podName);
    setView("describe");
    setDescribe("");
    setLogs([]);
    const r = await fetch(`/api/apps/${app.instance_name}/describe/${podName}`).catch(() => null);
    if (r?.ok) {
      const d = await r.json();
      setDescribe(d.output);
    }
  }

  async function showLogs(podName: string) {
    if (readerRef.current) { readerRef.current.cancel(); readerRef.current = null; }
    setView("logs");
    setLogs([]);
    const r = await fetch(`/api/apps/${app.instance_name}/logs/${podName}`);
    if (!r.body) return;
    const reader = r.body.getReader();
    readerRef.current = reader;
    const decoder = new TextDecoder();
    let buf = "";
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      const parts = buf.split("\n\n");
      buf = parts.pop() ?? "";
      for (const part of parts) {
        const line = part.startsWith("data: ") ? part.slice(6) : part;
        if (line.trim()) setLogs((l) => [...l, line]);
      }
    }
  }

  useEffect(() => () => { readerRef.current?.cancel(); }, []);

  return (
    <div style={{
      position: "fixed", inset: 0, background: "rgba(0,0,0,0.4)",
      display: "flex", alignItems: "center", justifyContent: "center", zIndex: 100,
    }}>
      <div style={{
        background: "#fff", borderRadius: 12, padding: "2rem",
        width: "100%", maxWidth: 700, maxHeight: "90vh", display: "flex", flexDirection: "column",
      }}>
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: "1rem" }}>
          <div>
            <div style={{ fontWeight: "bold", fontSize: "1.1rem" }}>{app.app_id}</div>
            <a href={app.tunnel_url} target="_blank" rel="noreferrer" style={{ fontSize: "0.8rem", color: "#7c3aed", fontFamily: "monospace" }}>{app.tunnel_url}</a>
          </div>
          <button onClick={onClose} style={{ background: "none", border: "none", cursor: "pointer", fontSize: "1.2rem" }}>✕</button>
        </div>

        <div style={{ display: "flex", flexWrap: "wrap", gap: "0.4rem", marginBottom: "1rem" }}>
          {pods.map((pod) => (
            <button
              key={pod.name}
              onClick={() => selectPod(pod.name)}
              style={{
                display: "flex", alignItems: "center", gap: "0.4rem",
                padding: "0.3rem 0.7rem", borderRadius: 6, cursor: "pointer", fontSize: "0.78rem",
                border: selectedPod === pod.name ? "2px solid #1a1a1a" : "1px solid #e5e7eb",
                background: selectedPod === pod.name ? "#f5f5f5" : "#fff",
              }}
            >
              <span style={{ width: 8, height: 8, borderRadius: "50%", background: phaseColor(pod), display: "inline-block" }} />
              {pod.name}
            </button>
          ))}
          {pods.length === 0 && <div style={{ fontSize: "0.8rem", color: "#999" }}>No pods found…</div>}
        </div>

        {selectedPod && (
          <>
            <div style={{ display: "flex", gap: "0.25rem", marginBottom: "0.5rem" }}>
              {(["describe", "logs"] as const).map((v) => (
                <button
                  key={v}
                  onClick={() => v === "logs" ? showLogs(selectedPod) : setView("describe")}
                  style={{
                    padding: "0.25rem 0.75rem", fontSize: "0.78rem", borderRadius: 4, cursor: "pointer",
                    background: view === v ? "#1a1a1a" : "#f5f5f5",
                    color: view === v ? "#fff" : "#333",
                    border: "none",
                  }}
                >
                  {v}
                </button>
              ))}
            </div>
            <div
              ref={outputRef}
              style={{
                flex: 1, background: "#111", color: "#d1fae5", borderRadius: 6,
                padding: "0.75rem", fontSize: "0.72rem", overflowY: "auto",
                fontFamily: "monospace", whiteSpace: "pre-wrap", minHeight: 300,
              }}
            >
              {view === "describe"
                ? (describe || <span style={{ color: "#666" }}>Loading…</span>)
                : logs.length === 0
                  ? <span style={{ color: "#666" }}>Waiting for logs…</span>
                  : logs.map((l, i) => <div key={i}>{l}</div>)
              }
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function InstalledCard({ app, onClick }: { app: InstalledApp; onClick: () => void }) {
  return (
    <div
      onClick={onClick}
      style={{
        border: "1px solid #e5e7eb", borderRadius: 8, padding: "1rem 1.25rem",
        display: "flex", justifyContent: "space-between", alignItems: "center", cursor: "pointer",
      }}
    >
      <div>
        <div style={{ fontWeight: "bold" }}>{app.app_id}</div>
        <div style={{ fontSize: "0.8rem", color: "#666", marginTop: 2 }}>{app.instance_name}</div>
      </div>
      <a
        href={app.tunnel_url}
        target="_blank"
        rel="noreferrer"
        onClick={(e) => e.stopPropagation()}
        style={{ fontSize: "0.82rem", color: "#1a1a1a", fontFamily: "monospace" }}
      >
        {app.tunnel_url}
      </a>
    </div>
  );
}

function InstallModal({ app, onClose }: { app: CatalogApp; onClose: () => void }) {
  const [instanceName, setInstanceName] = useState(app.id);
  const [formData, setFormData] = useState<object>({});
  const [log, setLog] = useState<string[]>([]);
  const [installing, setInstalling] = useState(false);
  const [done, setDone] = useState(false);
  const [error, setError] = useState("");
  const [tunnelDomain, setTunnelDomain] = useState("");
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    fetch("/api/tunnel/domain").then((r) => r.json()).then((d) => setTunnelDomain(d.domain)).catch(() => {});
  }, []);

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
      body: JSON.stringify({ instance_name: instanceName, config: formData }),
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
        width: "100%", maxWidth: 520, maxHeight: "90vh", overflowY: "auto",
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

        <Form
          schema={app.schema as never}
          uiSchema={app.uischema as never}
          validator={validator}
          formData={formData}
          onChange={({ formData: d }) => setFormData(d ?? {})}
          onSubmit={() => install()}
          widgets={{ TunnelWidget }}
          formContext={{ tunnelDomain }}
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
  const [detail, setDetail] = useState<InstalledApp | null>(null);

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
            {installed.map((a) => <InstalledCard key={a.instance_name} app={a} onClick={() => setDetail(a)} />)}
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
          </div>
        ))}
      </div>

      {detail && <AppDetailModal app={detail} onClose={() => setDetail(null)} />}

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

import { useEffect, useState } from "react";
import { HashRouter, Routes, Route, Navigate } from "react-router-dom";
import { Layout } from "@/components/Layout";
import { LoginPage } from "@/pages/LoginPage";
import { OverviewPage } from "@/pages/OverviewPage";
import { NodesPage } from "@/pages/NodesPage";
import { BackupsPage } from "@/pages/BackupsPage";
import {
  AppsPage,
  AppInstallPage,
  InstalledDetailPage,
} from "@/pages/AppsPage";
import { TerminalPage } from "@/pages/TerminalPage";
import { StoragePage } from "@/pages/StoragePage";

function AppRoutes({ onLogout }: { onLogout: () => void }) {
  return (
    <Routes>
      <Route path="/" element={<Navigate to="/overview" replace />} />
      <Route element={<Layout onLogout={onLogout} />}>
        <Route path="/overview" element={<OverviewPage />} />
        <Route path="/nodes" element={<NodesPage />} />
        <Route path="/backups" element={<BackupsPage />} />
        <Route path="/apps" element={<AppsPage />} />
        <Route path="/apps/:appId" element={<AppInstallPage />} />
        <Route
          path="/installed/:instanceName"
          element={<InstalledDetailPage />}
        />
        <Route path="/terminal" element={<TerminalPage />} />
        <Route path="/storage" element={<StoragePage />} />
      </Route>
    </Routes>
  );
}

export default function App() {
  const [loggedIn, setLoggedIn] = useState<boolean | null>(null);

  useEffect(() => {
    fetch("/api/status")
      .then((r) => setLoggedIn(r.status !== 401))
      .catch(() => setLoggedIn(true));
  }, []);

  if (loggedIn === null) return null;

  if (!loggedIn) {
    return <LoginPage onLogin={() => setLoggedIn(true)} />;
  }

  return (
    <HashRouter>
      <AppRoutes onLogout={() => setLoggedIn(false)} />
    </HashRouter>
  );
}

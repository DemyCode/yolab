import { useEffect, useState } from "react";
import { HashRouter, Routes, Route, Navigate } from "react-router-dom";
import { AnimatePresence, motion } from "framer-motion";
import { useLocation } from "react-router-dom";
import { Layout } from "@/components/Layout";
import { LoginPage } from "@/pages/LoginPage";
import { OverviewPage } from "@/pages/OverviewPage";
import { NodesPage } from "@/pages/NodesPage";
import { DisksPage } from "@/pages/DisksPage";
import {
  AppsPage,
  AppInstallPage,
  InstalledDetailPage,
} from "@/pages/AppsPage";
import { TerminalPage } from "@/pages/TerminalPage";

const pageTransition = {
  initial: { opacity: 0, y: 6 },
  animate: { opacity: 1, y: 0 },
  exit: { opacity: 0, y: -6 },
  transition: { duration: 0.18 },
} as const;

function AnimatedRoutes({ onLogout }: { onLogout: () => void }) {
  const location = useLocation();
  return (
    <AnimatePresence mode="wait">
      <motion.div
        key={location.pathname}
        {...pageTransition}
        className="h-full"
      >
        <Routes location={location}>
          <Route path="/" element={<Navigate to="/overview" replace />} />
          <Route element={<Layout onLogout={onLogout} />}>
            <Route path="/overview" element={<OverviewPage />} />
            <Route path="/nodes" element={<NodesPage />} />
            <Route path="/disks" element={<DisksPage />} />
            <Route path="/apps" element={<AppsPage />} />
            <Route path="/apps/:appId" element={<AppInstallPage />} />
            <Route
              path="/installed/:instanceName"
              element={<InstalledDetailPage />}
            />
            <Route path="/terminal" element={<TerminalPage />} />
          </Route>
        </Routes>
      </motion.div>
    </AnimatePresence>
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
      <AnimatedRoutes onLogout={() => setLoggedIn(false)} />
    </HashRouter>
  );
}

import { lazy, Suspense, useCallback, useEffect, useState } from "react";
import {
  BrowserRouter,
  Navigate,
  Route,
  Routes,
  useLocation,
} from "react-router-dom";
import { fromOldBoxPath } from "@/lib/routes";
import { AppShell } from "@/components/AppShell";
import { Spinner } from "@/components/ui/feedback";
import { LoginPage } from "@/pages/LoginPage";
import { HomePage } from "@/pages/HomePage";
import { AppsPage } from "@/pages/AppsPage";
import { InstallPage } from "@/pages/InstallPage";
import CustomAppPage from "@/pages/CustomAppPage";
import { AppDetailPage } from "@/pages/AppDetailPage";
import { SystemPage } from "@/pages/system/SystemPage";
import { SystemSubPage } from "@/pages/system/SystemSubPage";
import { api, setUnauthorizedHandler } from "@/lib/api";

const StoragePage = lazy(() =>
  import("@/pages/system/StoragePage").then((m) => ({ default: m.StoragePage })),
);
const BackupsPage = lazy(() =>
  import("@/pages/system/BackupsPage").then((m) => ({ default: m.BackupsPage })),
);
const NodesPage = lazy(() =>
  import("@/pages/system/NodesPage").then((m) => ({ default: m.NodesPage })),
);
const UpdatesPage = lazy(() =>
  import("@/pages/system/UpdatesPage").then((m) => ({ default: m.UpdatesPage })),
);
const TerminalPage = lazy(() =>
  import("@/pages/system/TerminalPage").then((m) => ({ default: m.TerminalPage })),
);
const LogsPage = lazy(() =>
  import("@/pages/system/LogsPage").then((m) => ({ default: m.LogsPage })),
);

function OldBoxPath() {
  const { pathname } = useLocation();
  return <Navigate to={fromOldBoxPath(pathname)} replace />;
}

function Loading() {
  return (
    <div className="flex justify-center py-24">
      <Spinner />
    </div>
  );
}

export default function App() {
  const [loggedIn, setLoggedIn] = useState<boolean | null>(null);

  const handleLogout = useCallback(() => setLoggedIn(false), []);

  useEffect(() => {
    setUnauthorizedHandler(handleLogout);
    return () => setUnauthorizedHandler(null);
  }, [handleLogout]);

  useEffect(() => {
    api
      .get("/api/status")
      .then(() => setLoggedIn(true))
      .catch((e: unknown) => {
        const unauthorized =
          typeof e === "object" && e !== null && "status" in e
            ? (e as { status: number }).status === 401
            : false;
        setLoggedIn(!unauthorized);
      });
  }, []);

  if (loggedIn === null) return null;
  if (!loggedIn) return <LoginPage onLogin={() => setLoggedIn(true)} />;

  return (
    <>
      <BrowserRouter>
        <Routes>
          <Route element={<AppShell onLogout={handleLogout} />}>
            <Route path="/" element={<HomePage />} />
            <Route path="/app/:instanceName" element={<AppDetailPage />} />
            <Route path="/add" element={<AppsPage />} />
            <Route path="/add/custom" element={<CustomAppPage />} />
            <Route path="/add/:appId" element={<InstallPage />} />
            <Route path="/system" element={<SystemPage />} />
            <Route
              path="/system/storage"
              element={
                <SystemSubPage
                  title="Storage"
                  subtitle="The disks your apps keep their data on."
                >
                  <Suspense fallback={<Loading />}>
                    <StoragePage />
                  </Suspense>
                </SystemSubPage>
              }
            />
            <Route
              path="/system/backups"
              element={
                <SystemSubPage
                  title="Backups"
                  subtitle="Copies of your data, kept somewhere else."
                >
                  <Suspense fallback={<Loading />}>
                    <BackupsPage />
                  </Suspense>
                </SystemSubPage>
              }
            />
            <Route
              path="/system/machines"
              element={
                <SystemSubPage
                  title="Machines"
                  subtitle="Every machine that makes up your home server."
                >
                  <Suspense fallback={<Loading />}>
                    <NodesPage />
                  </Suspense>
                </SystemSubPage>
              }
            />
            <Route
              path="/system/updates"
              element={
                <SystemSubPage
                  title="Updates"
                  subtitle="What version your machines are running."
                >
                  <Suspense fallback={<Loading />}>
                    <UpdatesPage />
                  </Suspense>
                </SystemSubPage>
              }
            />
            <Route
              path="/system/logs"
              element={
                <SystemSubPage
                  title="Logs"
                  subtitle="Everything the machine has been saying."
                >
                  <Suspense fallback={<Loading />}>
                    <LogsPage />
                  </Suspense>
                </SystemSubPage>
              }
            />
            <Route
              path="/system/terminal"
              element={
                <SystemSubPage
                  title="Terminal"
                  subtitle="Run commands directly on the machine."
                >
                  <Suspense fallback={<Loading />}>
                    <TerminalPage />
                  </Suspense>
                </SystemSubPage>
              }
            />
            <Route path="/box/*" element={<OldBoxPath />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </>
  );
}

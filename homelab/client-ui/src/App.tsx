import { lazy, Suspense, useCallback, useEffect, useState } from "react";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { AppShell } from "@/components/AppShell";
import { Spinner } from "@/components/ui/feedback";
import { LoginPage } from "@/pages/LoginPage";
import { HomePage } from "@/pages/HomePage";
import { DiscoverPage } from "@/pages/DiscoverPage";
import { InstallPage } from "@/pages/InstallPage";
import CustomAppPage from "@/pages/CustomAppPage";
import SearchPage from "@/pages/SearchPage";
import { AppDetailPage } from "@/pages/AppDetailPage";
import { BoxPage } from "@/pages/box/BoxPage";
import { BoxSubPage } from "@/pages/box/BoxSubPage";
import { api, setUnauthorizedHandler } from "@/lib/api";

// The operator pages are large, rarely opened, and not on the path to anything
// someone does daily — so they are not in the bundle that has to load before
// the home screen paints.
const StoragePage = lazy(() =>
  import("@/pages/box/StoragePage").then((m) => ({ default: m.StoragePage })),
);
const BackupsPage = lazy(() =>
  import("@/pages/box/BackupsPage").then((m) => ({ default: m.BackupsPage })),
);
const NodesPage = lazy(() =>
  import("@/pages/box/NodesPage").then((m) => ({ default: m.NodesPage })),
);
const SystemPage = lazy(() =>
  import("@/pages/box/SystemPage").then((m) => ({ default: m.SystemPage })),
);
const TerminalPage = lazy(() =>
  import("@/pages/box/TerminalPage").then((m) => ({ default: m.TerminalPage })),
);

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
    // Any 401 from anywhere returns to the sign-in screen, rather than each
    // page inventing its own handling and some of them inventing none.
    setUnauthorizedHandler(handleLogout);
    return () => setUnauthorizedHandler(null);
  }, [handleLogout]);

  useEffect(() => {
    api
      .get("/api/status")
      .then(() => setLoggedIn(true))
      // A network failure is not proof of being signed out — on a box that is
      // still booting, treating it as one would bounce the owner to a login
      // screen their password will not yet work against.
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
    <BrowserRouter>
      <Routes>
        <Route element={<AppShell onLogout={handleLogout} />}>
          <Route path="/" element={<HomePage />} />
          <Route path="/app/:instanceName" element={<AppDetailPage />} />
          <Route path="/add" element={<DiscoverPage />} />
          <Route path="/search" element={<SearchPage />} />
          <Route path="/add/custom" element={<CustomAppPage />} />
          <Route path="/add/:appId" element={<InstallPage />} />
          <Route path="/box" element={<BoxPage />} />
          <Route
            path="/box/storage"
            element={
              <BoxSubPage
                title="Storage"
                subtitle="The disks your apps keep their data on."
              >
                <Suspense fallback={<Loading />}>
                  <StoragePage />
                </Suspense>
              </BoxSubPage>
            }
          />
          <Route
            path="/box/backups"
            element={
              <BoxSubPage
                title="Backups"
                subtitle="Copies of your data, kept somewhere else."
              >
                <Suspense fallback={<Loading />}>
                  <BackupsPage />
                </Suspense>
              </BoxSubPage>
            }
          />
          <Route
            path="/box/machines"
            element={
              <BoxSubPage
                title="Machines"
                subtitle="Every machine that makes up your home server."
              >
                <Suspense fallback={<Loading />}>
                  <NodesPage />
                </Suspense>
              </BoxSubPage>
            }
          />
          <Route
            path="/box/system"
            element={
              <BoxSubPage
                title="Updates and system"
                subtitle="What version your machines are running."
              >
                <Suspense fallback={<Loading />}>
                  <SystemPage />
                </Suspense>
              </BoxSubPage>
            }
          />
          <Route
            path="/box/terminal"
            element={
              <BoxSubPage
                title="Terminal"
                subtitle="Run commands directly on the machine."
              >
                <Suspense fallback={<Loading />}>
                  <TerminalPage />
                </Suspense>
              </BoxSubPage>
            }
          />
          {/* Old bookmarks and the previous hash routes land somewhere sensible
              rather than on a blank page. */}
          <Route path="*" element={<Navigate to="/" replace />} />
        </Route>
      </Routes>
    </BrowserRouter>
  );
}

import { useEffect, useMemo, useState } from "react";
import { Link, useLocation, useNavigate, useParams } from "react-router-dom";
import { Check, Circle, Loader2, X } from "lucide-react";
import { Page } from "@/components/AppShell";
import { AppIconTile } from "@/components/AppIcon";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { EmptyState, Spinner } from "@/components/ui/feedback";
import { Field, Input, Select } from "@/components/ui/input";
import { Row, Section } from "@/components/ui/list";
import { api } from "@/lib/api";
import { DEFAULT_SIZE_GIB, type Folder } from "@/lib/folders";
import { generateSecret } from "@/lib/format";
import {
  defaultPlan,
  installOrder,
  sameApp,
  setupConfig,
  usedFolders,
  type AppPlan,
  type CatalogSetup,
  type FolderPlan,
  type Setup,
  type SetupPlan,
} from "@/lib/groups";
import { seedForm } from "@/lib/install";
import { configSchemaOf } from "@/lib/schema";
import { useApi } from "@/lib/useResource";
import type { AppInfo, CatalogApp } from "@/types/apps";

const NEW = "__new__";
const SKIP = "__skip__";

type StepState = "waiting" | "running" | "done" | "failed";

interface Step {
  label: string;
  state: StepState;
  error?: string;
}

function StepIcon({ state }: { state: StepState }) {
  if (state === "done") return <Check className="h-4 w-4 text-success" />;
  if (state === "failed") return <X className="h-4 w-4 text-danger" />;
  if (state === "running") {
    return <Loader2 className="h-4 w-4 animate-spin text-primary" />;
  }
  return <Circle className="h-4 w-4 text-fg-subtle" />;
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function SetupPage() {
  const { setupId } = useParams();
  const location = useLocation();
  const navigate = useNavigate();
  const fromFile = (location.state as { setup?: Setup } | null)?.setup;

  const setups = useApi<CatalogSetup[]>("setups", "/api/setups");
  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
  const apps = useApi<AppInfo[]>("apps", "/api/apps");
  const folders = useApi<Folder[]>("folders", "/api/folders");

  const setup: Setup | undefined =
    setupId === "file"
      ? fromFile
      : setups.data?.find((s) => s.id === setupId);

  const installed = useMemo(() => apps.data ?? [], [apps.data]);
  const [plan, setPlan] = useState<SetupPlan | null>(null);
  const [steps, setSteps] = useState<Step[] | null>(null);
  const [running, setRunning] = useState(false);

  const ready = Boolean(setup && apps.data && folders.data && catalog.data);
  useEffect(() => {
    if (ready && setup && !plan) {
      setPlan(defaultPlan(setup, installed, folders.data ?? []));
    }
  }, [ready, setup, plan, installed, folders.data]);

  if (!setup) {
    if (setups.loading) {
      return (
        <Page>
          <div className="flex justify-center py-20">
            <Spinner />
          </div>
        </Page>
      );
    }
    return (
      <Page>
        <EmptyState
          title="This setup is not available"
          body="It may have been removed from the catalog, or the file was not kept after a reload."
          action={
            <Link to="/add" className={buttonClass()}>
              Back to apps
            </Link>
          }
        />
      </Page>
    );
  }

  if (!plan) {
    return (
      <Page title={setup.title} subtitle={setup.tagline}>
        <div className="flex justify-center py-20">
          <Spinner />
        </div>
      </Page>
    );
  }

  const catalogApps = catalog.data ?? [];
  const order = installOrder(setup);
  const needed = usedFolders(setup, plan);

  function setFolder(key: string, next: FolderPlan) {
    setPlan((p) => p && { ...p, folders: { ...p.folders, [key]: next } });
  }

  function setApp(key: string, next: AppPlan) {
    setPlan((p) => p && { ...p, apps: { ...p.apps, [key]: next } });
  }

  function mark(i: number, state: StepState, error?: string) {
    setSteps((s) =>
      s ? s.map((step, j) => (j === i ? { ...step, state, error } : step)) : s,
    );
  }

  async function run(current: SetupPlan, s: Setup) {
    const title = current.title.trim() || s.title;
    const folderKeys = [...usedFolders(s, current)];
    const appKeys = installOrder(s).filter(
      (k) => current.apps[k]?.kind !== "skip",
    );
    const list: Step[] = [
      ...folderKeys.map((k) => {
        const f = current.folders[k];
        return {
          label:
            f?.kind === "new"
              ? `Create the folder ${f.title}`
              : `Use the folder ${f?.kind === "existing" ? f.name : k}`,
          state: "waiting" as const,
        };
      }),
      ...appKeys.map((k) => {
        const a = current.apps[k];
        const name =
          catalogApps.find((c) => c.id === s.apps[k].chart)?.name ?? k;
        return {
          label:
            a.kind === "existing"
              ? `Add your ${a.instance} to ${title}`
              : `Install ${name}`,
          state: "waiting" as const,
        };
      }),
    ];
    setSteps(list);
    setRunning(true);

    let i = 0;
    const folderNames: Record<string, string> = {};
    for (const key of folderKeys) {
      const f = current.folders[key];
      mark(i, "running");
      try {
        if (!f) {
          throw new Error(`the setup does not describe the folder ${key}`);
        }
        if (f.kind === "existing") {
          folderNames[key] = f.name;
        } else {
          const made = await api.post<{ name: string }>("/api/folders", {
            title: f.title,
            size_gib: DEFAULT_SIZE_GIB,
          });
          folderNames[key] = made.name;
        }
        mark(i, "done");
      } catch (e) {
        mark(i, "failed", message(e));
        setRunning(false);
        return;
      }
      i++;
    }

    let failed = false;
    const instances: Record<string, string> = {};
    for (const key of appKeys) {
      const a = current.apps[key];
      const app = s.apps[key];
      const group = { title, main: key === s.main };
      mark(i, "running");
      try {
        if (a.kind === "existing") {
          await api.put(`/api/apps/${encodeURIComponent(a.instance)}/group`, {
            group,
          });
          instances[key] = a.instance;
        } else {
          const chart = catalogApps.find((c) => c.id === app.chart);
          if (!chart) {
            throw new Error(`${app.chart} is not in your app catalog`);
          }
          const schema = configSchemaOf(chart.schema);
          const config = setupConfig(
            seedForm(schema, null, "fresh", generateSecret),
            app,
            folderNames,
            instances,
          );
          const begun = await api.post<{ instance_name: string }>(
            `/api/apps/${encodeURIComponent(app.chart)}`,
            { instance_name: key, config, group },
          );
          instances[key] = begun.instance_name;
        }
        mark(i, "done");
      } catch (e) {
        failed = true;
        mark(i, "failed", message(e));
      }
      i++;
    }
    setRunning(false);
    void apps.refresh();
    void folders.refresh();
    if (!failed) navigate("/");
  }

  return (
    <Page title={setup.title} subtitle={setup.tagline}>
      <p className="text-sm text-fg-muted">
        Each app below becomes a normal app, grouped together on your home
        screen. You can change, add or remove any of them later.
      </p>

      <div className="mt-6">
        <Field label="Name on your home screen" htmlFor="group-title">
          <Input
            id="group-title"
            value={plan.title}
            disabled={running || steps !== null}
            onChange={(e) => setPlan({ ...plan, title: e.target.value })}
          />
        </Field>
      </div>

      {Object.keys(setup.folders ?? {}).length > 0 && (
        <Section title="Folders">
          {Object.entries(setup.folders ?? {}).map(([key, f]) => {
            const p = plan.folders[key];
            const value = p?.kind === "existing" ? p.name : NEW;
            return (
              <Row
                key={key}
                label={f.title}
                detail={
                  needed.has(key)
                    ? "Where these apps keep and share their files"
                    : "Not needed: no new app uses it"
                }
                trailing={
                  <Select
                    value={value}
                    aria-label={`Folder for ${f.title}`}
                    disabled={running || steps !== null}
                    onChange={(e) =>
                      setFolder(
                        key,
                        e.target.value === NEW
                          ? { kind: "new", title: f.title }
                          : { kind: "existing", name: e.target.value },
                      )
                    }
                    className="w-56"
                  >
                    <option value={NEW}>New folder: {f.title}</option>
                    {(folders.data ?? []).map((existing) => (
                      <option key={existing.name} value={existing.name}>
                        Use {existing.title}
                      </option>
                    ))}
                  </Select>
                }
              />
            );
          })}
        </Section>
      )}

      <Section title="Apps">
        {order.map((key) => {
          const app = setup.apps[key];
          const chart = catalogApps.find((c) => c.id === app.chart);
          const p = plan.apps[key];
          const value =
            p.kind === "existing" ? p.instance : p.kind === "skip" ? SKIP : NEW;
          return (
            <Row
              key={key}
              label={
                <span className="flex items-center gap-3">
                  <AppIconTile
                    appId={app.chart}
                    icon={chart?.icon ?? "📦"}
                    name={chart?.name ?? app.chart}
                    size="sm"
                  />
                  <span>
                    {chart?.name ?? app.chart}
                    {key === setup.main && (
                      <span className="ml-2 text-xs text-fg-muted">
                        opens from your home screen
                      </span>
                    )}
                  </span>
                </span>
              }
              detail={!chart ? "Not in your app catalog" : undefined}
              trailing={
                <Select
                  value={value}
                  aria-label={`What to do with ${chart?.name ?? app.chart}`}
                  disabled={running || steps !== null}
                  onChange={(e) => {
                    const v = e.target.value;
                    setApp(
                      key,
                      v === NEW
                        ? { kind: "new" }
                        : v === SKIP
                          ? { kind: "skip" }
                          : { kind: "existing", instance: v },
                    );
                  }}
                  className="w-56"
                >
                  <option value={NEW}>Install a new one</option>
                  {sameApp(installed, app.chart).map((a) => (
                    <option key={a.instance_name} value={a.instance_name}>
                      Use your {a.instance_name}
                    </option>
                  ))}
                  <option value={SKIP}>Leave it out</option>
                </Select>
              }
            />
          );
        })}
      </Section>

      {steps && (
        <Section title="Progress">
          {steps.map((step, i) => (
            <Row
              key={i}
              label={
                <span className="flex items-center gap-2">
                  <StepIcon state={step.state} />
                  {step.label}
                </span>
              }
              detail={
                step.error ? (
                  <span className="text-danger">{step.error}</span>
                ) : undefined
              }
            />
          ))}
        </Section>
      )}

      <div className="mt-8 flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
        {steps && !running ? (
          <Link to="/" className={buttonClass()}>
            Go to your home screen
          </Link>
        ) : (
          <Button
            onClick={() => void run(plan, setup)}
            loading={running}
            disabled={order.every((k) => plan.apps[k].kind === "skip")}
          >
            Install {setup.title}
          </Button>
        )}
      </div>
    </Page>
  );
}

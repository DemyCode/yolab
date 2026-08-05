import { useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ArrowLeft, Check, ChevronDown, ExternalLink } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { Card } from "@/components/ui/card";
import {
  Field,
  GeneratedSecret,
  Input,
  Select,
  Toggle,
} from "@/components/ui/input";
import { Banner, Spinner } from "@/components/ui/feedback";
import { api, streamEvents } from "@/lib/api";
import { useResource } from "@/lib/useResource";
import { generateSecret } from "@/lib/format";
import { nextInstanceName } from "@/lib/apps";
import { AppIcon } from "@/components/AppIcon";
import { taglineFor } from "@/catalog/meta";
import { cn } from "@/lib/utils";
import type { AppInfo, CatalogApp, DomainResponse } from "@/types/apps";

// ── Schema ──────────────────────────────────────────────────────────────────
// Every chart in the catalog describes its install form with the same tiny
// slice of JSON Schema: 125 string properties, one boolean, two enums, and the
// custom `format: tunnel`. That is small enough to render deliberately, which
// is why this replaced the generic JSON-Schema form renderer the old UI used.
//
// An auto-generated form is *definitionally* schema-shaped — it shows field
// names, types and validation messages, because that is all it has. It cannot
// know that `subdomain` is "the web address", that `app_secret` should never
// have been asked for, or that `storage_size` is not a first question. Those
// judgements are what makes an install feel considered, and they have to be
// written down somewhere.

interface SchemaProp {
  type?: string;
  title?: string;
  default?: unknown;
  description?: string;
  format?: string;
  enum?: string[];
  minLength?: number;
  maxLength?: number;
}

interface ConfigSchema {
  properties?: Record<string, SchemaProp>;
  required?: string[];
}

function configSchema(schema: object | undefined): ConfigSchema {
  const s = schema as { properties?: { config?: ConfigSchema } } | undefined;
  return s?.properties?.config ?? {};
}

/** Secret-ish names, used to decide what we can generate on the user's behalf. */
const SECRET_NAME = /pass|secret|key|token/i;

type FieldKind = "address" | "secret" | "required" | "optional";

function classify(
  name: string,
  prop: SchemaProp,
  required: Set<string>,
): FieldKind {
  if (prop.format === "tunnel") return "address";
  const hasDefault = prop.default !== undefined && prop.default !== "";
  if (required.has(name) && !hasDefault) {
    return SECRET_NAME.test(name) ? "secret" : "required";
  }
  return "optional";
}

/**
 * Turn the install stream into something a person can read.
 *
 * local-api streams Helm's own output, which is accurate and unreadable. We do
 * not invent progress percentages we cannot know — the bar stays indeterminate
 * — but we do name the phase, because "Setting up storage" answers the only
 * question anyone has while waiting, which is whether it is stuck.
 */
function phaseFrom(line: string): string | null {
  const l = line.toLowerCase();
  if (l.includes("namespace") || l.includes("staging")) return "Getting ready";
  if (l.includes("tunnel") || l.includes("record"))
    return "Reserving your web address";
  if (l.includes("pending-install") || l.includes("helm")) return "Installing";
  if (l.includes("deployed") || l.includes("status:")) return "Almost there";
  return null;
}

export function InstallPage() {
  const { appId } = useParams<{ appId: string }>();
  const navigate = useNavigate();

  const catalog = useResource<CatalogApp[]>("catalog", () =>
    api.get("/api/apps/catalog"),
  );
  const domain = useResource<DomainResponse>("domain", () =>
    api.get("/api/tunnel/domain"),
  );
  const apps = useResource<AppInfo[]>("apps", () => api.get("/api/apps"));

  const app = catalog.data?.find((a) => a.id === appId);
  const schema = useMemo(() => configSchema(app?.schema), [app?.schema]);
  const required = useMemo(
    () => new Set(schema.required ?? []),
    [schema.required],
  );

  const [showAdvanced, setShowAdvanced] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [phase, setPhase] = useState("Getting ready");
  const [log, setLog] = useState<string[]>([]);
  const [showLog, setShowLog] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  // A free name for this install. `nextcloud` the first time, `nextcloud-2`
  // the next — the name is both the Kubernetes namespace and the default web
  // address, so it has to be unused and a valid DNS label.
  const installedOfThisApp = (apps.data ?? []).filter(
    (a) => a.app_id === appId,
  );
  const suggestedName = nextInstanceName(appId ?? "", apps.data ?? []);
  const [nameEdit, setNameEdit] = useState<string | null>(null);
  const instanceName = nameEdit ?? suggestedName;
  const isCopy = instanceName !== appId;

  const addressKey = useMemo(
    () =>
      Object.entries(schema.properties ?? {}).find(
        ([, p]) => p.format === "tunnel",
      )?.[0],
    [schema.properties],
  );

  // Generated once per chart and deliberately *not* recomputed when anything
  // else on the form changes — otherwise typing in the name field would mint a
  // new password on every keystroke, including after the user copied one.
  const secretDefaults = useMemo(() => {
    const out: Record<string, unknown> = {};
    for (const [name, prop] of Object.entries(schema.properties ?? {})) {
      if (classify(name, prop, required) === "secret") {
        out[name] = generateSecret(Math.max(24, prop.minLength ?? 0));
      }
    }
    return out;
  }, [schema.properties, required]);

  // Everything else's starting point, derived rather than copied into state on
  // mount: seeding from an effect renders an empty form first and the real one
  // second, which on a slow catalog fetch is a visible flash of blank fields.
  const baseDefaults = useMemo(() => {
    const seeded: Record<string, unknown> = {};
    for (const [name, prop] of Object.entries(schema.properties ?? {})) {
      if (classify(name, prop, required) === "secret") continue;
      if (prop.default !== undefined) seeded[name] = prop.default;
      else if (prop.type === "boolean") seeded[name] = false;
      else seeded[name] = "";
    }
    return seeded;
  }, [schema.properties, required]);

  // Only what the user actually changed, layered on top.
  const [edits, setEdits] = useState<Record<string, unknown>>({});
  const values = useMemo(
    () => ({
      ...baseDefaults,
      ...secretDefaults,
      // The address tracks the name, so a second copy does not silently try to
      // claim the first one's subdomain — until the user sets one explicitly,
      // at which point `edits` wins.
      ...(addressKey ? { [addressKey]: instanceName } : {}),
      ...edits,
    }),
    [baseDefaults, secretDefaults, addressKey, instanceName, edits],
  );
  const setValue = (name: string, v: unknown) =>
    setEdits((e) => ({ ...e, [name]: v }));

  const fields = Object.entries(schema.properties ?? {});
  const addressField = fields.find(
    ([n, p]) => classify(n, p, required) === "address",
  );
  const secretFields = fields.filter(
    ([n, p]) => classify(n, p, required) === "secret",
  );
  const requiredFields = fields.filter(
    ([n, p]) => classify(n, p, required) === "required",
  );
  const optionalFields = fields.filter(
    ([n, p]) =>
      classify(n, p, required) === "optional" && p.format !== "tunnel",
  );

  const subdomain =
    addressField && typeof values[addressField[0]] === "string"
      ? (values[addressField[0]] as string)
      : "";
  const fullUrl =
    subdomain && domain.data?.domain
      ? `https://${subdomain}.${domain.data.domain}`
      : null;

  const nameTaken = (apps.data ?? []).some(
    (a) => a.instance_name === instanceName,
  );
  const blocking =
    !instanceName ||
    nameTaken ||
    requiredFields.some(([n]) => !String(values[n] ?? "").trim());

  async function install() {
    if (!app) return;
    setInstalling(true);
    setError(null);
    setLog([]);
    setPhase("Getting ready");

    // Drop empty values that the schema has no default for, rather than
    // sending `""`. An empty string is a real value to Helm and would override
    // whatever the chart's own values.yaml sets — so a field we only rendered
    // because it exists would silently blank out a working default.
    const payload = Object.fromEntries(
      Object.entries(values).filter(([name, v]) => {
        if (v !== "") return true;
        return schema.properties?.[name]?.default !== undefined;
      }),
    );

    const result = await streamEvents(
      `/api/apps/${app.id}`,
      {
        method: "POST",
        body: JSON.stringify({ instance_name: instanceName, config: payload }),
      },
      (line) => {
        setLog((l) => [...l, line]);
        const next = phaseFrom(line);
        if (next) setPhase(next);
      },
    );

    setInstalling(false);
    if (result.ok) setDone(true);
    else {
      setError(result.error ?? "Something went wrong during the install.");
      setShowLog(true);
    }
  }

  if (catalog.loading && !app) {
    return (
      <Page>
        <div className="flex justify-center py-20">
          <Spinner />
        </div>
      </Page>
    );
  }

  if (!app) {
    return (
      <Page title="App not found">
        <p className="text-sm text-fg-muted">
          That app is not in the catalog any more.
        </p>
        <Link
          to="/add"
          className={cn(buttonClass({ variant: "secondary" }), "mt-4")}
        >
          Back to apps
        </Link>
      </Page>
    );
  }

  // ── Success ───────────────────────────────────────────────────────────────
  if (done) {
    return (
      <Page>
        <div className="flex flex-col items-center py-10 text-center">
          <div className="mb-5 flex h-16 w-16 items-center justify-center rounded-tile bg-success-soft">
            <Check className="h-8 w-8 text-success" />
          </div>
          <h1 className="text-2xl font-semibold text-fg">
            {app.name} is ready
          </h1>
          <p className="mt-2 max-w-sm text-sm text-fg-muted">
            It may take another minute to finish starting the first time.
          </p>

          {secretFields.length > 0 && (
            <Card className="mt-6 w-full max-w-md p-5 text-left">
              <p className="mb-3 text-sm font-medium text-fg">
                Save these before you leave this page
              </p>
              <div className="space-y-3">
                {secretFields.map(([name, prop]) => (
                  <div key={name}>
                    <div className="text-xs text-fg-muted">
                      {prop.title ?? name}
                    </div>
                    <code className="block break-all font-mono text-sm text-fg">
                      {String(values[name] ?? "")}
                    </code>
                  </div>
                ))}
              </div>
            </Card>
          )}

          <div className="mt-7 flex w-full max-w-md flex-col gap-2 sm:flex-row">
            {fullUrl && (
              <a
                href={fullUrl}
                target="_blank"
                rel="noopener noreferrer"
                className={cn(buttonClass(), "flex-1")}
              >
                Open {app.name}
                <ExternalLink className="h-4 w-4" />
              </a>
            )}
            <Button
              variant="secondary"
              className="flex-1"
              onClick={() => navigate("/")}
            >
              Back to my apps
            </Button>
          </div>
        </div>
      </Page>
    );
  }

  // ── Installing ────────────────────────────────────────────────────────────
  if (installing) {
    return (
      <Page>
        <div className="flex flex-col items-center py-14 text-center">
          <div className="mb-6 flex h-16 w-16 items-center justify-center rounded-tile bg-surface-2">
            <AppIcon
              icon={app.icon}
              name={app.name}
              className="h-10 w-10 text-4xl"
            />
          </div>
          <h1 className="text-xl font-semibold text-fg">
            Setting up {app.name}
          </h1>
          <p className="mt-2 text-sm text-fg-muted">{phase}…</p>

          <div className="mt-6 h-1.5 w-full max-w-xs overflow-hidden rounded-full bg-surface-2">
            <div className="h-full w-1/3 animate-pulse rounded-full bg-primary" />
          </div>

          <p className="mt-6 max-w-sm text-sm text-fg-muted">
            This usually takes a minute or two. You can leave this page — it
            keeps going.
          </p>

          <button
            onClick={() => setShowLog((s) => !s)}
            className="mt-6 text-sm text-fg-subtle underline underline-offset-2 hover:text-fg"
          >
            {showLog ? "Hide" : "Show"} technical details
          </button>
          {showLog && (
            <pre className="mt-3 max-h-64 w-full overflow-auto rounded-xl bg-surface-2 p-3 text-left font-mono text-xs leading-relaxed text-fg-muted">
              {log.join("\n")}
            </pre>
          )}
        </div>
      </Page>
    );
  }

  // ── Form ──────────────────────────────────────────────────────────────────
  return (
    <Page>
      <Link
        to="/add"
        className="mb-5 inline-flex items-center gap-1.5 text-sm text-fg-muted hover:text-fg"
      >
        <ArrowLeft className="h-4 w-4" />
        All apps
      </Link>

      <div className="mb-7 flex items-center gap-4">
        <div className="flex h-14 w-14 shrink-0 items-center justify-center rounded-tile bg-surface-2">
          <AppIcon
            icon={app.icon}
            name={app.name}
            className="h-8 w-8 text-3xl"
          />
        </div>
        <div className="min-w-0">
          <h1 className="text-2xl font-semibold tracking-tight text-fg">
            {app.name}
          </h1>
          <p className="mt-0.5 text-sm text-fg-muted">{taglineFor(app)}</p>
        </div>
      </div>

      {isCopy && (
        <Banner
          tone="info"
          title={
            installedOfThisApp.length === 1
              ? `You already have ${app.name}`
              : `You already have ${installedOfThisApp.length} copies of ${app.name}`
          }
          className="mb-5"
        >
          This adds another, completely separate one — its own storage, its own
          login, its own web address. Nothing about the existing{" "}
          {installedOfThisApp.length === 1 ? "one" : "ones"} changes.
        </Banner>
      )}

      {error && (
        <Banner
          tone="error"
          title="The install did not finish"
          className="mb-5"
        >
          {error}
        </Banner>
      )}

      {app.repo !== "official" && (
        <Banner
          tone="warning"
          title={`This app comes from "${app.repo}"`}
          className="mb-5"
        >
          You added this source yourself. Apps from outside the official catalog
          can do anything on your box, so only install ones you trust.
        </Banner>
      )}

      <Card className="divide-y divide-border">
        {fullUrl && (
          <div className="p-5">
            <div className="text-sm font-medium text-fg">Web address</div>
            <p className="mt-1 break-all font-mono text-sm text-primary">
              {fullUrl}
            </p>
          </div>
        )}

        {secretFields.length > 0 && (
          <div className="space-y-5 p-5">
            {secretFields.map(([name, prop]) => (
              <GeneratedSecret
                key={name}
                label={prop.title ?? "Password"}
                minLength={prop.minLength}
                value={String(values[name] ?? "")}
                onChange={(v) => setValue(name, v)}
              />
            ))}
          </div>
        )}

        {requiredFields.length > 0 && (
          <div className="space-y-5 p-5">
            {requiredFields.map(([name, prop]) => (
              <Field
                key={name}
                label={prop.title ?? name}
                help={prop.description}
              >
                <Input
                  value={String(values[name] ?? "")}
                  onChange={(e) => setValue(name, e.target.value)}
                />
              </Field>
            ))}
          </div>
        )}
      </Card>

      {/* Always available: even a chart with no options of its own has a name,
          and for a second copy that is the thing you want to change. */}
      <div className="mt-4">
        <button
          onClick={() => setShowAdvanced((a) => !a)}
          className="flex items-center gap-1.5 text-sm text-fg-muted hover:text-fg"
          aria-expanded={showAdvanced}
        >
          Settings
          <ChevronDown
            className={cn(
              "h-4 w-4 transition-transform",
              showAdvanced && "rotate-180",
            )}
          />
        </button>

        {showAdvanced && (
          <Card className="mt-3 space-y-5 p-5">
            <Field
              label="Name"
              help={
                nameTaken
                  ? undefined
                  : "What this copy is called on your box. The web address follows it unless you set one below."
              }
              error={
                nameTaken ? "You already have something with that name." : null
              }
            >
              <Input
                value={instanceName}
                onChange={(e) =>
                  // Doubles as a Kubernetes namespace, so it is restricted to
                  // what a DNS label allows.
                  setNameEdit(
                    e.target.value.toLowerCase().replace(/[^a-z0-9-]/g, ""),
                  )
                }
              />
            </Field>

            {addressField && (
              <Field
                label="Web address"
                help={`Your app will live at ${subdomain || "…"}.${domain.data?.domain ?? ""}`}
              >
                <Input
                  value={subdomain}
                  onChange={(e) =>
                    // A subdomain is a DNS label, so anything the keyboard
                    // can produce that DNS cannot is dropped as it is typed
                    // rather than rejected after they press Install.
                    setValue(
                      addressField[0],
                      e.target.value.toLowerCase().replace(/[^a-z0-9-]/g, ""),
                    )
                  }
                />
              </Field>
            )}

            {optionalFields.map(([name, prop]) => {
              if (prop.type === "boolean") {
                return (
                  <Toggle
                    key={name}
                    label={prop.title ?? name}
                    help={prop.description}
                    checked={Boolean(values[name])}
                    onChange={(v) => setValue(name, v)}
                  />
                );
              }
              if (prop.enum) {
                return (
                  <Field
                    key={name}
                    label={prop.title ?? name}
                    help={prop.description}
                  >
                    <Select
                      value={String(values[name] ?? "")}
                      onChange={(e) => setValue(name, e.target.value)}
                    >
                      {prop.enum.map((opt) => (
                        <option key={opt} value={opt}>
                          {opt}
                        </option>
                      ))}
                    </Select>
                  </Field>
                );
              }
              return (
                <Field
                  key={name}
                  label={prop.title ?? name}
                  help={prop.description}
                >
                  <Input
                    value={String(values[name] ?? "")}
                    onChange={(e) => setValue(name, e.target.value)}
                  />
                </Field>
              );
            })}
          </Card>
        )}
      </div>

      <div className="mt-7">
        <Button
          full
          size="lg"
          onClick={() => void install()}
          disabled={blocking}
        >
          Install {app.name}
        </Button>
      </div>
    </Page>
  );
}

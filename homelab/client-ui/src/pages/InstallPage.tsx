import { useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ArrowLeft, Check, ExternalLink } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { Card } from "@/components/ui/card";
// GeneratedSecret / Select / Toggle are gone from here: RJSF renders those
// through the widgets in components/form, chosen by the chart's own uiSchema.
import { Field, Input } from "@/components/ui/input";
import { Banner, Spinner } from "@/components/ui/feedback";
import { api, streamEvents } from "@/lib/api";
import { useResource } from "@/lib/useResource";
import { generateSecret } from "@/lib/format";
import Form from "@rjsf/core";
import type { RJSFSchema } from "@rjsf/utils";
import validator from "@rjsf/validator-ajv8";
import { templates, widgets } from "@/components/form/registry";
import { nextInstanceName } from "@/lib/apps";
import { AppIconTile } from "@/components/AppIcon";
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

/**
 * The API already hands us the config subtree, not the whole values schema:
 * `read_chart` (routers/apps.rs) stores `values.schema.json`'s
 * `properties.config` as the app's `schema`. Unwrapping `properties.config`
 * again here found nothing and fell through to `{}`, so the form rendered ZERO
 * fields and posted `config: {}` — every app installed with none of its options
 * set, and no subdomain, which left the gateway crash-looping on a blank FQDN.
 *
 * Tolerant of both shapes so a chart or an older node that still sends the full
 * values schema keeps working.
 */
function configSchema(schema: object | undefined): ConfigSchema {
  if (!schema) return {};
  const s = schema as ConfigSchema & { properties?: { config?: ConfigSchema } };
  const nested = s.properties?.config;
  if (nested && typeof nested === "object" && "properties" in nested) {
    return nested;
  }
  return s.properties ? s : {};
}

// Field classification by regex is gone. Which field is a password, which is
// the web address, which wants autofocus — the chart says so in its uiSchema
// (`ui:widget: PasswordWidget`, `TunnelWidget`, `ui:autofocus`), and RJSF reads
// it. Guessing from names both ignored what the author declared and quietly
// disagreed with it.

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

  const cached = catalog.data?.find((a) => a.id === appId);

  // Re-pull this one chart before rendering the form.
  //
  // The node syncs charts hourly, so a chart published minutes ago still serves
  // its previous schema — and a field the author just added is simply absent.
  // That looks like a broken change rather than a stale copy, and it is only
  // ever noticed by the person who published it, staring at a form missing the
  // option they wrote.
  //
  // Best-effort: if the registry is unreachable the cached chart is still
  // perfectly installable, so `fresh` stays null and the cached entry renders.
  const [fresh, setFresh] = useState<CatalogApp | null>(null);
  useEffect(() => {
    if (!appId) return;
    let cancelled = false;
    void (async () => {
      try {
        const r = await api.post<{ app: CatalogApp | null }>(
          `/api/apps/catalog/${appId}/refresh`,
        );
        if (!cancelled && r?.app) setFresh(r.app);
      } catch {
        // Offline, or the chart is only in the bundled catalog. Either way the
        // cached copy is what we would have shown anyway.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [appId]);

  const app = fresh ?? cached;
  const schema = useMemo(() => configSchema(app?.schema), [app?.schema]);
  const required = useMemo(
    () => new Set(schema.required ?? []),
    [schema.required],
  );

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

  // ── What RJSF renders ─────────────────────────────────────────────────────
  //
  // The schema comes straight from the chart. The uiSchema is the chart's own
  // `yolab.io/uischema` annotation, with two things layered on that only this
  // page knows:
  //
  //   - the tunnel field's live-URL domain, which is a property of this node
  //   - a password field's initial value, generated rather than left blank
  //
  // Both used to be inferred by matching field names. Now the chart declares
  // `ui:widget` and this supplies what the widget needs.
  const addressKey = useMemo(
    () =>
      Object.entries(schema.properties ?? {}).find(
        ([, p]) => p.format === "tunnel",
      )?.[0],
    [schema.properties],
  );

  // Cast at the boundary: ConfigSchema is a deliberately narrow local view of
  // the handful of JSON Schema this catalog uses, while RJSF wants the full
  // JSONSchema7. The value really is a JSON Schema — it came from the chart —
  // so this is a widening, not a lie.
  const rjsfSchema = useMemo(
    () => ({ type: "object", ...schema }) as RJSFSchema,
    [schema],
  );

  const rjsfUiSchema = useMemo(() => {
    const chartUi = (app?.uischema ?? {}) as Record<string, unknown>;
    const ui: Record<string, unknown> = { ...chartUi };
    if (addressKey) {
      const existing = (ui[addressKey] ?? {}) as Record<string, unknown>;
      ui[addressKey] = {
        ...existing,
        "ui:options": {
          ...((existing["ui:options"] as object) ?? {}),
          domain: domain.data?.domain ?? "",
        },
      };
    }
    return ui;
  }, [app?.uischema, addressKey, domain.data?.domain]);

  // Seeded once per chart, then owned by the form. Regenerating on every
  // keystroke would mint a new password after the user had copied one.
  const [formData, setFormData] = useState<Record<string, unknown>>({});
  const seeded = useRef<string | null>(null);
  useEffect(() => {
    if (!appId || !schema.properties || seeded.current === appId) return;
    seeded.current = appId;
    const seed: Record<string, unknown> = {};
    for (const [name, prop] of Object.entries(schema.properties)) {
      const widget = ((
        app?.uischema as Record<string, Record<string, unknown>>
      )?.[name] ?? {})["ui:widget"];
      if (widget === "PasswordWidget") {
        seed[name] = generateSecret(Math.max(24, prop.minLength ?? 0));
      } else if (prop.default !== undefined) {
        seed[name] = prop.default;
      }
    }
    setFormData(seed);
  }, [appId, schema.properties, app?.uischema]);

  // The address tracks the instance name until the user sets one explicitly,
  // so a second copy does not silently try to claim the first one's subdomain.
  const values = useMemo(() => {
    if (!addressKey) return formData;
    return formData[addressKey]
      ? formData
      : { ...formData, [addressKey]: instanceName };
  }, [formData, addressKey, instanceName]);

  // Shown once on the success screen: a generated password is worth copying
  // before install and worthless afterwards. Which fields those are comes from
  // the chart's uiSchema, not from guessing at names.
  const generatedSecrets = useMemo<[string, string][]>(() => {
    const ui = (app?.uischema ?? {}) as Record<string, Record<string, unknown>>;
    return Object.entries(schema.properties ?? {})
      .filter(([n]) => ui[n]?.["ui:widget"] === "PasswordWidget")
      .map(([n, p]) => [n, p.title ?? n]);
  }, [app?.uischema, schema.properties]);

  const subdomain =
    addressKey && typeof values[addressKey] === "string"
      ? (values[addressKey] as string)
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
    // Required per the schema itself, rather than a locally-derived list.
    [...required].some((n) => !String(values[n] ?? "").trim());

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
    // Drop empty values the schema has no default for, rather than sending "".
    // An empty string is a real value to Helm and would override whatever the
    // chart's own values.yaml sets — so a field we only rendered because it
    // exists would silently blank out a working default.
    //
    // Fields hidden by a conditional (if/then) are simply absent from formData,
    // so a password typed and then switched off never reaches the release —
    // RJSF prunes them, which is what the bespoke `showIf` was doing by hand.
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

          {generatedSecrets.length > 0 && (
            <Card className="mt-6 w-full max-w-md p-5 text-left">
              <p className="mb-3 text-sm font-medium text-fg">
                Save these before you leave this page
              </p>
              <div className="space-y-3">
                {generatedSecrets.map(([name, title]) => (
                  <div key={name}>
                    <div className="text-xs text-fg-muted">{title}</div>
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
          <AppIconTile
            appId={app.id}
            icon={app.icon}
            name={app.name}
            className="mb-6"
          />
          <h1 className="font-display text-2xl text-fg">
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
        <AppIconTile appId={app.id} icon={app.icon} name={app.name} />
        <div className="min-w-0">
          <h1 className="font-display text-3xl text-fg">{app.name}</h1>
          <p className="mt-0.5 text-sm text-fg-muted">{taglineFor(app)}</p>
          {/* The last moment before committing to an install is exactly when
              someone wants to check what this actually is. */}
          {app.home && (
            <a
              href={app.home}
              target="_blank"
              rel="noreferrer noopener"
              className="mt-1 inline-flex items-center gap-1 text-sm text-fg-subtle transition-colors hover:text-primary hover:underline"
            >
              <ExternalLink className="h-3.5 w-3.5" />
              Visit the project's website
            </a>
          )}
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
          can do anything on your machines, so only install ones you trust.
        </Banner>
      )}

      <Card className="divide-y divide-border">
        {/* The address used to be printed here as well as under the Subdomain
            field, so the same URL appeared twice on one page. TunnelWidget shows
            it inline now — next to the box you edit, which is where it answers
            the question — so this hand-written copy is gone. */}
        {/* Every option the chart declares, rendered by RJSF from its own
            schema and uiSchema. The catalog already ships uiSchema — 55
            TunnelWidget, 20 PasswordWidget, 5 ui:autofocus — which the previous
            hand-rolled renderer ignored, re-deriving the same intent by
            matching field names against /pass|secret|key|token/. A chart author
            writing `ui:widget: PasswordWidget` is no longer overruled by a
            regex, and conditional fields come from the schema's own if/then
            rather than a bespoke `showIf`. */}
        <div className="p-5">
          <Form
            schema={rjsfSchema}
            uiSchema={rjsfUiSchema}
            formData={formData}
            validator={validator}
            widgets={widgets}
            templates={templates}
            liveValidate={false}
            showErrorList={false}
            onChange={(e) => setFormData(e.formData ?? {})}
          >
            {/* RJSF renders its own submit button unless given children. The
                install action lives at the bottom of the page, not inside the
                form. */}
            <></>
          </Form>
        </div>

        <div className="space-y-5 p-5">
          <Field
            label="Name"
            help={
              nameTaken
                ? undefined
                : "What this copy is called on your home server. The web address follows it unless you set one below."
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
        </div>
      </Card>

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

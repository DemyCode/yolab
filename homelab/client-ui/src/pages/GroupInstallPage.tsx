import { useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { Page } from "@/components/AppShell";
import { AppIconTile } from "@/components/AppIcon";
import { GroupForm } from "@/components/GroupForm";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { EmptyState, Spinner } from "@/components/ui/feedback";
import { api } from "@/lib/api";
import { generateSecret } from "@/lib/format";
import { groupNameFor, type GroupRecord } from "@/lib/groups";
import { seedForm } from "@/lib/install";
import { configSchemaOf } from "@/lib/schema";
import { useApi } from "@/lib/useResource";
import type { CatalogApp } from "@/types/apps";

export function GroupInstallPage() {
  const { groupId = "" } = useParams();
  const navigate = useNavigate();
  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
  const groups = useApi<GroupRecord[]>("groups", "/api/groups");
  const entry = catalog.data?.find(
    (a) => a.id === groupId && a.kind === "group",
  );
  const config = useMemo(() => configSchemaOf(entry?.schema), [entry?.schema]);

  const [formData, setFormData] = useState<Record<string, unknown> | null>(
    null,
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const seeded = useMemo(
    () =>
      config.properties
        ? seedForm(config, null, "fresh", generateSecret)
        : null,
    [config],
  );
  const values = formData ?? seeded;
  const suggested = useMemo(
    () =>
      groups.data
        ? groupNameFor(
            groupId,
            groups.data.map((g) => g.name),
          )
        : null,
    [groups.data, groupId],
  );
  const chosenName = suggested;

  if (!entry) {
    if (catalog.loading) {
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
          title="This group is not in your catalog"
          body="It may have been removed, or it comes from a source you no longer use."
          action={
            <Link to="/add" className={buttonClass()}>
              Back to apps
            </Link>
          }
        />
      </Page>
    );
  }

  async function install() {
    if (!entry || !chosenName || !values) return;
    setBusy(true);
    setError(null);
    try {
      await api.post("/api/groups", {
        chart: entry.id,
        repo: entry.repo,
        name: chosenName,
        config: values,
      });
      navigate(`/group/${chosenName}`);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  }

  return (
    <Page>
      <div className="flex items-center gap-4">
        <AppIconTile appId={entry.id} icon={entry.icon} name={entry.name} />
        <div>
          <h1 className="font-display text-2xl text-fg">{entry.name}</h1>
          <p className="text-sm text-fg-muted">{entry.tagline}</p>
        </div>
      </div>
      <p className="mt-4 text-sm text-fg-muted">{entry.description}</p>
      <p className="mt-2 text-sm text-fg-muted">
        Each app it installs is a normal app: you can open, change or remove it
        on its own. The group remembers your choices so you can change them
        later.
      </p>

      <section className="mt-8">
        <h2 className="mb-2 px-1 text-sm font-semibold text-fg-muted">
          Your choices
        </h2>
        {entry.schema && values ? (
          <GroupForm
            schema={entry.schema}
            formData={values}
            onChange={setFormData}
          />
        ) : (
          <p className="text-sm text-danger">
            This group&apos;s form could not be read. Its catalog may be missing
            one of the apps it uses.
          </p>
        )}
      </section>

      {error && <p className="mt-4 text-sm text-danger">{error}</p>}

      <div className="mt-8 flex justify-end">
        <Button
          onClick={() => void install()}
          loading={busy}
          disabled={!chosenName || !values || !entry.schema}
        >
          Install {entry.name}
        </Button>
      </div>
    </Page>
  );
}

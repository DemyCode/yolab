import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { Check, Circle, Loader2, X } from "lucide-react";
import { Page } from "@/components/AppShell";
import { GroupForm } from "@/components/GroupForm";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { Banner, EmptyState, Spinner } from "@/components/ui/feedback";
import { Row, Section } from "@/components/ui/list";
import { ConfirmDialog } from "@/components/ui/sheet";
import { api } from "@/lib/api";
import {
  installedByGroup,
  memberRows,
  settingUp,
  type GroupView,
  type MemberStatus,
} from "@/lib/groups";
import { useApi } from "@/lib/useResource";

function StateIcon({ state }: { state: MemberStatus["state"] }) {
  if (state === "done") return <Check className="h-4 w-4 text-success" />;
  if (state === "failed") return <X className="h-4 w-4 text-danger" />;
  if (state === "working") {
    return <Loader2 className="h-4 w-4 animate-spin text-primary" />;
  }
  return <Circle className="h-4 w-4 text-fg-subtle" />;
}

const STATE_WORDS: Record<MemberStatus["state"], string> = {
  waiting: "Waiting its turn",
  working: "Being set up…",
  done: "Ready",
  failed: "Could not be set up",
};

type Removal = "keep" | "everything" | null;

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function GroupPage() {
  const { name = "" } = useParams();
  const navigate = useNavigate();
  const group = useApi<GroupView>(
    `group:${name}`,
    `/api/groups/${encodeURIComponent(name)}`,
    { pollMs: 3_000 },
  );
  const [editing, setEditing] = useState<Record<string, unknown> | null>(
    null,
  );
  const [removal, setRemoval] = useState<Removal>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const g = group.data;
  if (!g) {
    if (group.loading) {
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
          title="This group does not exist"
          body="It may have been removed. Its apps, if any are left, are on your home screen."
          action={
            <Link to="/" className={buttonClass()}>
              Home
            </Link>
          }
        />
      </Page>
    );
  }

  const rows = memberRows(g);
  const working = settingUp(g);

  async function save() {
    if (!editing) return;
    setBusy(true);
    setError(null);
    try {
      await api.put(`/api/groups/${encodeURIComponent(name)}`, {
        config: editing,
      });
      setEditing(null);
      await group.refresh();
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  }

  async function remove(which: Removal) {
    if (!g) return;
    setBusy(true);
    setError(null);
    try {
      if (which === "everything") {
        for (const instance of installedByGroup(g)) {
          await api.del(`/api/apps/${encodeURIComponent(instance)}`);
        }
      }
      await api.del(`/api/groups/${encodeURIComponent(name)}`);
      navigate("/");
    } catch (e) {
      setError(message(e));
      setBusy(false);
    }
  }

  return (
    <Page title={g.title} subtitle={`${g.chart} ${g.version}`}>
      {working && (
        <Banner tone="info" title="Setting up">
          Apps are installed one after another. You can leave this page; it
          keeps going.
        </Banner>
      )}
      {g.left.length > 0 && (
        <Banner tone="info" title="No longer part of this group">
          {g.left.join(", ")} stayed installed. Remove them from their own page
          if you no longer need them.
        </Banner>
      )}

      <Section title="Apps">
        {rows.map((row) => (
          <Row
            key={row.key}
            label={
              <span className="flex items-center gap-2">
                <StateIcon state={row.state} />
                {row.instance}
                {g.reused.includes(row.key) && (
                  <span className="text-xs text-fg-muted">you already had it</span>
                )}
              </span>
            }
            detail={
              row.message ? (
                <span className="text-danger">{row.message}</span>
              ) : (
                STATE_WORDS[row.state]
              )
            }
            onClick={() => navigate(`/app/${row.instance}`)}
          />
        ))}
      </Section>

      <Section
        title="Your choices"
        action={
          !editing && (
            <Button
              variant="secondary"
              size="sm"
              disabled={working || !g.schema}
              onClick={() => setEditing({ ...g.values })}
            >
              Change
            </Button>
          )
        }
      >
        {editing ? (
          <div className="p-4">
            <GroupForm
              schema={g.schema}
              formData={editing}
              onChange={setEditing}
            />
            <p className="mt-3 text-sm text-fg-muted">
              Apps you keep get the new settings; apps your new choices add are
              installed; apps they drop leave the group but stay installed.
            </p>
            <div className="mt-4 flex justify-end gap-2">
              <Button
                variant="secondary"
                onClick={() => setEditing(null)}
                disabled={busy}
              >
                Cancel
              </Button>
              <Button onClick={() => void save()} loading={busy}>
                Apply
              </Button>
            </div>
          </div>
        ) : (
          <Row
            label="Passwords stay hidden"
            detail="Change opens the same form as when you installed it."
          />
        )}
      </Section>

      <Section title="Remove">
        <Row
          label="Remove the group, keep its apps"
          detail="They go back to being separate apps on your home screen."
          onClick={() => setRemoval("keep")}
          disabled={working}
        />
        <Row
          label="Remove the group and its apps"
          danger
          detail="Deletes every app it installed, with their files. Apps you already had stay."
          onClick={() => setRemoval("everything")}
          disabled={working}
        />
      </Section>

      {error && <p className="mt-4 text-sm text-danger">{error}</p>}

      <ConfirmDialog
        open={removal !== null}
        onClose={() => setRemoval(null)}
        onConfirm={() => void remove(removal)}
        title={
          removal === "everything"
            ? `Remove ${g.title} and its apps?`
            : `Remove the group ${g.title}?`
        }
        body={
          removal === "everything"
            ? `${installedByGroup(g).join(", ") || "Nothing"} will be deleted with their files. Backups you already have are kept.`
            : "Its apps keep running exactly as they are."
        }
        confirmLabel={removal === "everything" ? "Remove everything" : "Remove"}
        destructive={removal === "everything"}
        busy={busy}
      />
    </Page>
  );
}

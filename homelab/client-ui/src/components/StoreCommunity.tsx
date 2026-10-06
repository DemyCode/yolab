import { useState } from "react";
import { ExternalLink, Star } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Switch } from "@/components/ui/input";
import { Row, Section } from "@/components/ui/list";
import { api, ApiError } from "@/lib/api";
import { formatDateTime, relativeTime } from "@/lib/format";
import { githubUrl, hasCommunity, type StoreComment } from "@/lib/store";
import { useApi } from "@/lib/useResource";
import { cn } from "@/lib/utils";
import type { CatalogApp } from "@/types/apps";

const COMMENT_MAX = 1000;

function reasonOf(e: unknown): string {
  return e instanceof ApiError
    ? e.message
    : "The YoLab platform did not answer.";
}

function YourRating({ appId }: { appId: string }) {
  const mine = useApi<{ stars: number | null }>(
    `store-rating-${appId}`,
    `/api/store/apps/${appId}/rating`,
  );
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const current = mine.data?.stars ?? null;

  async function choose(stars: number) {
    const next = stars === current ? null : stars;
    setError(null);
    try {
      await api.put(`/api/store/apps/${appId}/rating`, { stars: next });
      mine.mutate({ stars: next });
      setSaved(true);
    } catch (e) {
      setError(reasonOf(e));
    }
  }

  return (
    <Row
      label="Your rating"
      detail={
        error ??
        (saved
          ? "Saved"
          : current
            ? "Click your rating again to remove it"
            : "")
      }
      trailing={
        <div className="flex shrink-0 gap-0.5" role="group" aria-label="Rate">
          {[1, 2, 3, 4, 5].map((n) => (
            <button
              key={n}
              type="button"
              onClick={() => void choose(n)}
              aria-label={`${n} star${n === 1 ? "" : "s"}`}
              aria-pressed={current !== null && n <= current}
              className="rounded-control p-1 text-fg-subtle transition-colors hover:text-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary"
            >
              <Star
                className={cn(
                  "h-5 w-5",
                  current !== null && n <= current && "fill-current text-fg",
                )}
                aria-hidden
              />
            </button>
          ))}
        </div>
      }
    />
  );
}

export function AppAbout({ app }: { app: CatalogApp }) {
  const repo = githubUrl(app);
  const community = hasCommunity(app);
  if (!repo && !community) return null;

  return (
    <Section title="About">
      {repo && (
        <a
          href={repo}
          target="_blank"
          rel="noreferrer noopener"
          className="flex items-center gap-4 px-5 py-4 transition-colors hover:bg-surface-2 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-primary"
        >
          <div className="min-w-0 flex-1">
            <div className="text-sm font-medium text-fg">On GitHub</div>
            <div className="mt-0.5 font-mono text-sm tabular-nums text-fg-muted">
              {app.github}
            </div>
          </div>
          <ExternalLink className="h-4 w-4 shrink-0 text-fg-subtle" />
        </a>
      )}
      {community && <YourRating appId={app.id} />}
    </Section>
  );
}

function CommentRow({
  comment,
  onChanged,
}: {
  comment: StoreComment;
  onChanged: () => void;
}) {
  const [note, setNote] = useState<string | null>(null);

  async function remove() {
    try {
      await api.del(`/api/store/comments/${comment.id}`);
      onChanged();
    } catch (e) {
      setNote(reasonOf(e));
    }
  }

  async function report() {
    try {
      await api.post(`/api/store/comments/${comment.id}/report`);
      setNote("Reported. It is hidden once three people report it.");
    } catch (e) {
      setNote(reasonOf(e));
    }
  }

  return (
    <Row
      label={
        <span className="flex flex-wrap items-baseline gap-x-2">
          <span>{comment.author}</span>
          <span
            className="font-mono text-xs text-fg-subtle"
            title={formatDateTime(comment.created_at)}
          >
            {relativeTime(comment.created_at)}
          </span>
          {comment.installed && (
            <span className="text-xs text-fg-subtle">installed it</span>
          )}
        </span>
      }
      detail={
        <>
          <span className="whitespace-pre-line text-fg">{comment.body}</span>
          {note && <span className="mt-1 block">{note}</span>}
        </>
      }
      trailing={
        comment.mine ? (
          <button
            type="button"
            onClick={() => void remove()}
            className="shrink-0 self-start text-sm text-danger hover:underline"
          >
            Delete
          </button>
        ) : (
          <button
            type="button"
            onClick={() => void report()}
            className="shrink-0 self-start text-sm text-fg-subtle hover:text-fg"
          >
            Report
          </button>
        )
      }
    />
  );
}

export function AppComments({ app }: { app: CatalogApp }) {
  const list = useApi<StoreComment[]>(
    hasCommunity(app) ? `store-comments-${app.id}` : null,
    `/api/store/apps/${app.id}/comments`,
  );
  const [draft, setDraft] = useState("");
  const [posting, setPosting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (!hasCommunity(app)) return null;

  async function post() {
    setPosting(true);
    setError(null);
    try {
      await api.post(`/api/store/apps/${app.id}/comments`, { body: draft });
      setDraft("");
      await list.refresh();
    } catch (e) {
      setError(reasonOf(e));
    } finally {
      setPosting(false);
    }
  }

  const comments = list.data ?? [];
  const tooLong = draft.trim().length > COMMENT_MAX;

  return (
    <Section title="What people say">
      {list.error && !list.data ? (
        <Row label="Comments are not available right now" detail={list.error} />
      ) : comments.length === 0 && !list.loading ? (
        <Row
          label="No comments yet"
          detail="Installed it? Say how it went for you."
        />
      ) : (
        comments.map((c) => (
          <CommentRow
            key={c.id}
            comment={c}
            onChanged={() => void list.refresh()}
          />
        ))
      )}
      <div className="flex flex-col gap-2 px-5 py-4 sm:flex-row sm:items-start">
        <textarea
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          rows={2}
          maxLength={COMMENT_MAX + 50}
          placeholder="How did it go?"
          aria-label="Your comment"
          className="w-full flex-1 rounded-control border border-border-strong bg-surface px-3.5 py-2 text-sm text-fg placeholder:text-fg-subtle focus:border-primary focus:outline-none focus:ring-2 focus:ring-primary/20"
        />
        <Button
          variant="secondary"
          onClick={() => void post()}
          disabled={posting || !draft.trim() || tooLong}
        >
          {posting ? "Posting…" : "Post"}
        </Button>
      </div>
      {(error || tooLong) && (
        <p className="px-5 pb-4 text-sm text-danger" role="alert">
          {tooLong ? `A comment is at most ${COMMENT_MAX} characters.` : error}
        </p>
      )}
    </Section>
  );
}

export function StoreSharing() {
  const settings = useApi<{ share_installs: boolean }>(
    "store-settings",
    "/api/store/settings",
    { persist: false },
  );
  const [note, setNote] = useState<string | null>(null);
  const on = settings.data?.share_installs ?? true;

  async function change(next: boolean) {
    setNote(null);
    try {
      await api.put("/api/store/settings", { share_installs: next });
      settings.mutate({ share_installs: next });
      setNote("Saved");
    } catch (e) {
      setNote(reasonOf(e));
    }
  }

  return (
    <Card className="mb-4 overflow-hidden p-0">
      <Row
        label="Count my installs in the app store"
        detail={
          note ??
          "When you install an app from the YoLab catalog, it is counted once for your account. Nothing else about you or your data is sent."
        }
        trailing={
          <Switch
            checked={on}
            onChange={(v) => void change(v)}
            label="Count my installs in the app store"
            disabled={!settings.data}
          />
        }
      />
    </Card>
  );
}

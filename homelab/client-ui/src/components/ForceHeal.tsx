import { useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { CheckCircle, Circle, RefreshCw } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { Banner } from "@/components/ui/feedback";
import { Input } from "@/components/ui/input";
import { Sheet } from "@/components/ui/sheet";
import { api } from "@/lib/api";
import { cn } from "@/lib/utils";
import {
  HEAL_PROBLEM_LABELS,
  HEAL_STEP_LABELS,
  useHealStatus,
  type Heal,
  type HealStatus,
} from "@/lib/heal";

/** A finished heal stays on the page this long, so its outcome can be read. */
const SHOW_FINISHED_FOR_SECS = 24 * 3600;

function recentlyFinished(heal: Heal | null): boolean {
  return (
    heal !== null &&
    !heal.running &&
    heal.finished_at !== null &&
    Date.now() / 1000 - heal.finished_at < SHOW_FINISHED_FOR_SECS
  );
}

/**
 * The home page banner while something is wrong or a heal runs. It only points
 * the way: the decision is made on the Storage page, where the details are.
 */
export function HealBanner({ className }: { className?: string }) {
  const status = useHealStatus(20_000).data;
  if (!status) return null;
  const heal = status.heal;
  const link = (label: string) => (
    <Link
      to="/box/storage"
      className={buttonClass({ size: "sm", variant: "secondary" })}
    >
      {label}
    </Link>
  );
  if (heal?.running) {
    return (
      <Banner
        tone="info"
        title="Your home server is being healed"
        className={className}
        action={link("See progress")}
      >
        {HEAL_STEP_LABELS[heal.step]}. Your apps can be added back from backup
        once it finishes.
      </Banner>
    );
  }
  // A new problem outranks the note that the last heal finished.
  if (status.problems.length === 0) {
    return recentlyFinished(heal) ? (
      <Banner
        tone="info"
        title="Your home server was healed"
        className={className}
        action={link("Details")}
      >
        Add your apps back with “Add from backup”.
      </Banner>
    ) : null;
  }
  return (
    <Banner
      tone="error"
      title={HEAL_PROBLEM_LABELS[status.problems[0]]}
      className={className}
      action={link("Look at it")}
    >
      If a machine is only restarting or a disk can be plugged back in, do that
      — everything comes back on its own. Otherwise the Storage page can heal
      the cluster without it.
    </Banner>
  );
}

function StepList({ heal }: { heal: Heal }) {
  const at = heal.steps.indexOf(heal.step);
  return (
    <ol className="space-y-2.5">
      {heal.steps.map((step, i) => {
        const state = !heal.running
          ? "done"
          : i < at
            ? "done"
            : i === at
              ? "current"
              : "pending";
        return (
          <li key={step} className="flex gap-3">
            {state === "done" ? (
              <CheckCircle className="mt-0.5 h-5 w-5 shrink-0 text-success" />
            ) : state === "current" ? (
              <RefreshCw className="mt-0.5 h-5 w-5 shrink-0 animate-spin text-primary" />
            ) : (
              <Circle className="mt-0.5 h-5 w-5 shrink-0 text-fg-subtle" />
            )}
            <div className="min-w-0">
              <p
                className={cn(
                  "text-sm",
                  state === "current" && "font-medium text-fg",
                  state === "done" && "text-fg-muted",
                  state === "pending" && "text-fg-subtle",
                )}
              >
                {HEAL_STEP_LABELS[step]}
              </p>
              {state === "current" && heal.waiting && (
                <p className="mt-0.5 text-xs text-fg-muted">{heal.waiting}</p>
              )}
            </div>
          </li>
        );
      })}
    </ol>
  );
}

function HealProgress({
  heal,
  action,
}: {
  heal: Heal;
  /** Shown under the steps: starting again, when this heal cannot finish. */
  action?: ReactNode;
}) {
  return (
    <Card>
      <CardContent className="space-y-4 pt-5 pb-5">
        <div>
          <p className="text-base font-medium text-fg">
            {heal.running
              ? `Healing the cluster from ${heal.driver}`
              : "The cluster was healed"}
          </p>
          <p className="mt-1 text-sm text-fg-muted">
            {heal.running
              ? "This page updates by itself. Machines restart along the way, so it may stop answering for a few minutes."
              : "Storage and the cluster are as fresh as a new installation. Add your apps back from backup on the home page."}
          </p>
        </div>
        <StepList heal={heal} />
        {heal.removed_machines.length > 0 && (
          <p className="text-sm text-fg-muted">
            Removed:{" "}
            <span className="text-fg">{heal.removed_machines.join(", ")}</span>.
            To use them again, install them again.
          </p>
        )}
        {!heal.running && (
          <Link to="/" className={buttonClass({ size: "sm" })}>
            Add apps from backup
          </Link>
        )}
        {action}
      </CardContent>
    </Card>
  );
}

function HealDialog({
  open,
  onClose,
  status,
  onStarted,
}: {
  open: boolean;
  onClose: () => void;
  status: HealStatus;
  onStarted: () => void;
}) {
  const { plan } = status;
  const [confirmed, setConfirmed] = useState<Record<string, string>>({});
  const [phrase, setPhrase] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const machinesConfirmed = plan.remove_machines.every(
    (m) => confirmed[m]?.trim() === m,
  );
  const ready = machinesConfirmed && phrase.trim() === "HEAL";

  async function start() {
    setBusy(true);
    setError(null);
    try {
      await api.post("/api/heal", { remove_machines: plan.remove_machines });
      onStarted();
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not start the heal");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Sheet
      open={open}
      onClose={onClose}
      title="Force heal the cluster?"
      wide
      footer={
        <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
          <Button variant="secondary" onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button
            variant="danger"
            onClick={() => void start()}
            disabled={!ready}
            loading={busy}
          >
            Delete everything and heal
          </Button>
        </div>
      }
    >
      <div className="space-y-4 text-sm text-fg-muted">
        <p>
          The cluster is rebuilt from what still answers, as if it were freshly
          installed.{" "}
          <span className="font-medium text-fg">
            Every app and every stored file is deleted.
          </span>{" "}
          Afterwards, add your apps back from backup on the home page. This
          cannot be undone.
        </p>
        <ul className="list-disc space-y-1 pl-5">
          {plan.remove_machines.length > 0 && (
            <li>
              Removed for good:{" "}
              <span className="text-fg">{plan.remove_machines.join(", ")}</span>
              . They must be installed again to rejoin.
            </li>
          )}
          <li>
            Every disk that does not answer is forgotten and switched off.
          </li>
          {plan.reset_kubernetes && (
            <li>
              {status.survey.me} becomes the only member of the cluster&apos;s
              control plane.
            </li>
          )}
          <li>
            Restarted:{" "}
            <span className="text-fg">
              {[...plan.restart_machines, `${status.survey.me} (last)`].join(
                ", ",
              )}
            </span>
          </li>
        </ul>
        {plan.remove_machines.length > 0 && (
          <div className="space-y-2 rounded-card border border-danger/25 bg-danger-soft p-3">
            <p className="text-fg">
              Make sure each of these machines is really gone — powered off or
              broken. If one is only disconnected and comes back, you will have
              two separate clusters. Type each name to confirm.
            </p>
            {plan.remove_machines.map((m) => (
              <Input
                key={m}
                placeholder={m}
                value={confirmed[m] ?? ""}
                onChange={(e) =>
                  setConfirmed((c) => ({ ...c, [m]: e.target.value }))
                }
                aria-label={`Type ${m} to confirm`}
              />
            ))}
          </div>
        )}
        <div className="space-y-2">
          <p className="text-fg">
            Type <span className="font-mono font-semibold">HEAL</span> to
            continue.
          </p>
          <Input
            value={phrase}
            onChange={(e) => setPhrase(e.target.value)}
            aria-label="Type HEAL to continue"
          />
        </div>
        {error && <p className="text-danger">{error}</p>}
      </div>
    </Sheet>
  );
}

/** The Storage page section: what is wrong, the FORCE HEAL button, and progress. */
export function ForceHealCard() {
  // Not faster: every answer probes each machine that does not answer, which
  // takes seconds by itself.
  const status = useHealStatus(10_000);
  const [confirming, setConfirming] = useState(false);
  const s = status.data;
  if (!s) return null;
  const heal = s.heal;

  if (heal?.running) {
    // Past the restart only Kubernetes steps are left, and they can wait
    // forever on a machine that died meanwhile. The server accepts a new heal
    // from there, so offer one.
    const stuck =
      (heal.step === "forget_nodes" || heal.step === "remove_apps") &&
      s.problems.length > 0 &&
      !s.refusal;
    return (
      <HealProgress
        heal={heal}
        action={
          stuck && (
            <>
              <Button variant="danger" onClick={() => setConfirming(true)}>
                FORCE HEAL again
              </Button>
              <HealDialog
                open={confirming}
                onClose={() => setConfirming(false)}
                status={s}
                onStarted={() => void status.refresh()}
              />
            </>
          )
        }
      />
    );
  }
  // A finished heal is shown until something is wrong again — never in place of
  // the button a new problem needs.
  if (s.problems.length === 0) {
    return heal && recentlyFinished(heal) ? <HealProgress heal={heal} /> : null;
  }

  const gone = s.survey.machines.filter((m) => !m.answers);
  return (
    <Card className="border-danger/30 bg-danger-soft">
      <CardContent className="space-y-3 pt-5 pb-5">
        <ul className="space-y-1 text-sm font-medium text-danger">
          {s.problems.map((p) => (
            <li key={p}>{HEAL_PROBLEM_LABELS[p]}</li>
          ))}
        </ul>
        <div className="space-y-1 text-sm text-fg-muted">
          {gone.length > 0 && (
            <p>
              Not answering:{" "}
              <span className="text-fg">
                {gone.map((m) => m.name).join(", ")}
              </span>
            </p>
          )}
          {s.survey.lost_groups !== null && s.survey.lost_groups > 0 && (
            <p>
              {s.survey.lost_groups} group
              {s.survey.lost_groups === 1 ? "" : "s"} of files have no copy on a
              disk that answers.
            </p>
          )}
          <p>
            If a machine is only restarting or a disk can be plugged back in, do
            that instead — everything comes back on its own.
          </p>
        </div>
        {s.refusal ? (
          <p className="text-sm text-fg">{s.refusal}</p>
        ) : (
          <Button variant="danger" onClick={() => setConfirming(true)}>
            FORCE HEAL
          </Button>
        )}
        <HealDialog
          open={confirming}
          onClose={() => setConfirming(false)}
          status={s}
          onStarted={() => void status.refresh()}
        />
      </CardContent>
    </Card>
  );
}

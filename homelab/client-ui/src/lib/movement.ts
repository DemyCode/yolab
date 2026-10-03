import type { Movement, MovementJob } from "@/types/storage";
import { useApi } from "@/lib/useResource";

export type MovementTone = "calm" | "warning" | "error";

export interface JobCopy {
  job: MovementJob;
  label: string;
  note: string | null;
  tone: MovementTone;
}

export interface MovementCopy {
  headline: string;
  safety: string | null;
  tone: MovementTone;
  jobs: JobCopy[];
}

function list(names: string[]): string {
  if (names.length <= 1) return names[0] ?? "";
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

function copiesWord(copies: number | null): string {
  if (copies === 2) return "a second copy";
  if (copies === 3) return "a third copy";
  return "another copy";
}

function named(refs: { node: string; name: string }[]): string {
  return list(
    refs.map((d) =>
      /\bdisk$/i.test(d.name)
        ? `the ${d.name} on ${d.node}`
        : `the ${d.name} disk on ${d.node}`,
    ),
  );
}

function moveLabel(m: Movement, yours: boolean): string {
  const files = yours ? "your files" : "files";
  switch (m.move_reason) {
    case "draining":
      return `Moving ${files} off ${named(m.draining)}`;
    case "filling":
      return `Spreading ${files} onto ${named(m.filling)}`;
    case "resizing":
      return `Regrouping ${files} so your disks can hold more`;
    default:
      return yours
        ? "Evening out space across your disks"
        : "Evening out space across disks";
  }
}

function cloneLabel(m: Movement): string {
  return m.clones > 1
    ? `Copying files for ${m.clones} apps`
    : "Copying files for an app";
}

function jobCopy(job: MovementJob, m: Movement): JobCopy {
  switch (job.kind) {
    case "move":
      return { job, label: moveLabel(m, false), note: null, tone: "calm" };
    case "add_copies":
      return {
        job,
        label: `Adding ${copiesWord(m.copies)} of every file`,
        note: m.copies
          ? `You chose ${m.copies} copies. Until this finishes, each file still has the copies it had before.`
          : "Until this finishes, each file still has the copies it had before.",
        tone: "calm",
      };
    case "rebuild":
      return {
        job,
        label: m.waiting_for.length
          ? `Rebuilding copies that were on ${list(m.waiting_for)}`
          : "Rebuilding missing copies",
        note: null,
        tone: "warning",
      };
    case "clone":
      return {
        job,
        label: cloneLabel(m),
        note: "Duplicating or restoring an app copies its files inside your storage.",
        tone: "calm",
      };
    case "free_space":
      return {
        job,
        label: "Freeing space from deleted snapshots",
        note: null,
        tone: "calm",
      };
    case "repair":
      return {
        job,
        label: m.repairing
          ? "Repairing damaged copies"
          : "Damaged copies found",
        note: m.repairing
          ? null
          : "A routine check found copies that don't match. They are put right by a repair.",
        tone: "warning",
      };
  }
}

function headlineFor(jobs: JobCopy[], m: Movement): string {
  if (jobs.length > 1) return "Reorganising your files";
  const only = jobs[0]?.job.kind;
  if (only === "move") return moveLabel(m, true);
  if (only === "add_copies")
    return `Adding ${copiesWord(m.copies)} of every file`;
  if (jobs[0]) return jobs[0].label;
  return "Rearranging your files across your disks";
}

export function isVisible(m: Movement | undefined): m is Movement {
  return !!m && m.state !== "settled" && m.state !== "unknown";
}

export function movementCopy(m: Movement): MovementCopy | null {
  const jobs = (m.jobs ?? []).map((j) => jobCopy(j, m));
  const has = (kind: MovementJob["kind"]) =>
    jobs.some((j) => j.job.kind === kind);
  switch (m.state) {
    case "working":
      if (has("rebuild"))
        return {
          headline: headlineFor(jobs, m),
          safety:
            "Some files have fewer copies than you chose until this finishes. Don't unplug any disk or machine.",
          tone: "warning",
          jobs,
        };
      if (has("repair"))
        return {
          headline: headlineFor(jobs, m),
          safety:
            "Some copies were found damaged. Your apps keep working; don't unplug any disk or machine until they're repaired.",
          tone: "warning",
          jobs,
        };
      if (has("add_copies"))
        return {
          headline: headlineFor(jobs, m),
          safety: null,
          tone: "calm",
          jobs,
        };
      return {
        headline: headlineFor(jobs, m),
        safety: "Your files are safe and your apps keep working meanwhile.",
        tone: "calm",
        jobs,
      };
    case "unavailable":
      return {
        headline: m.waiting_for.length
          ? `Waiting for ${list(m.waiting_for)}`
          : "Some of your files can't be reached",
        safety: m.waiting_for.length
          ? "Some files can't be opened until it's back. Apps that use them may pause."
          : jobs.length
            ? "Some files can't be opened until enough copies are made. Apps that use them may pause."
            : "Some files can't be opened right now. Apps that use them may pause.",
        tone: "error",
        jobs,
      };
    case "no_room":
      return {
        headline: "Not enough room to finish reorganising your files",
        safety: "Add a disk, or free some space, and it carries on by itself.",
        tone: "error",
        jobs,
      };
    case "restarting":
      return {
        headline: m.waiting_for.length
          ? `${list(m.waiting_for)} is restarting`
          : "A machine is restarting",
        safety:
          "Nothing is being moved. Its files are available again as soon as it's back.",
        tone: "calm",
        jobs: [],
      };
    default:
      return null;
  }
}

export function overallProgress(m: Movement): number | null {
  const jobs = m.jobs ?? [];
  if (jobs.length === 0 || jobs.every((j) => j.progress === null)) return null;
  const total = jobs.reduce((s, j) => s + j.to_move_bytes, 0);
  if (total > 0) return jobs.reduce((s, j) => s + j.moved_bytes, 0) / total;
  const measured = jobs.filter((j) => j.progress !== null);
  return measured.reduce((s, j) => s + (j.progress ?? 0), 0) / measured.length;
}

export function jobOf(
  m: Movement | undefined,
  kind: MovementJob["kind"],
): MovementJob | undefined {
  return m?.jobs?.find((j) => j.kind === kind);
}

export function timeLeft(secs: number | null): string {
  if (secs === null) return "Estimating time left…";
  if (secs < 60) return "less than a minute left";
  const minutes = Math.round(secs / 60);
  if (minutes < 10) return `about ${minutes} min left`;
  if (minutes < 60) return `about ${Math.round(minutes / 5) * 5} min left`;
  const hours = Math.round(minutes / 60);
  if (hours < 48)
    return hours === 1 ? "about an hour left" : `about ${hours} hours left`;
  const days = Math.round(hours / 24);
  return `about ${days} days left`;
}

export function percent(progress: number | null): number {
  if (progress === null) return 0;
  return Math.max(0, Math.min(100, progress * 100));
}

export function needsAttentionEverywhere(m: Movement | undefined): boolean {
  return !!m && (m.state === "unavailable" || m.state === "no_room");
}

export function useMovement() {
  return useApi<Movement>("storage-movement-v3", "/api/storage/movement", {
    pollMs: 10_000,
  });
}

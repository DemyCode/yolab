import type { Movement } from "@/types/storage";
import { useApi } from "@/lib/useResource";

export type MovementTone = "calm" | "warning" | "error";

export interface MovementCopy {
  headline: string;
  safety: string;
  tone: MovementTone;
  showsProgress: boolean;
}

function list(names: string[]): string {
  if (names.length <= 1) return names[0] ?? "";
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

export function isVisible(m: Movement | undefined): m is Movement {
  return !!m && m.state !== "settled" && m.state !== "unknown";
}

export function movementCopy(m: Movement): MovementCopy | null {
  switch (m.state) {
    case "moving": {
      const disks = m.draining.map((d) => `the ${d.name} disk on ${d.node}`);
      return {
        headline: disks.length
          ? `Moving your files off ${list(disks)}`
          : "Rearranging your files across your disks",
        safety: "Your files are safe and your apps keep working meanwhile.",
        tone: "calm",
        showsProgress: true,
      };
    }
    case "rebuilding":
      return {
        headline: "Rebuilding copies of your files",
        safety:
          "Some files have only one copy until this finishes. Don't unplug any disk or machine.",
        tone: "warning",
        showsProgress: true,
      };
    case "unavailable":
      return {
        headline: m.waiting_for.length
          ? `Waiting for ${list(m.waiting_for)}`
          : "Some of your files can't be reached",
        safety:
          "Some files can't be opened until it's back. Apps that use them may pause.",
        tone: "error",
        showsProgress: false,
      };
    case "no_room":
      return {
        headline: "Not enough room to finish moving your files",
        safety: "Add a disk, or free some space, and it carries on by itself.",
        tone: "error",
        showsProgress: true,
      };
    case "restarting":
      return {
        headline: m.waiting_for.length
          ? `${list(m.waiting_for)} is restarting`
          : "A machine is restarting",
        safety:
          "Nothing is being moved. Its files are available again as soon as it's back.",
        tone: "calm",
        showsProgress: false,
      };
    default:
      return null;
  }
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
  return useApi<Movement>("storage-movement", "/api/storage/movement", {
    pollMs: 10_000,
  });
}

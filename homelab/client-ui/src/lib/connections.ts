import type { ServiceInstance } from "@/lib/services";
import type { AppInfo } from "@/types/apps";

export type Connection = Record<string, unknown>;

export type ConnectionChoice =
  | { kind: "own" }
  | { kind: "installed"; namespace: string }
  | { kind: "missing"; namespace: string }
  | { kind: "typed" };

export function connectionChoice(
  value: Connection,
  installed: ServiceInstance[],
  canRunOwn: boolean,
  typing: boolean,
): ConnectionChoice {
  if (typing) return { kind: "typed" };
  const from = typeof value.from === "string" ? value.from : "";
  if (from) {
    return installed.some((s) => s.namespace === from)
      ? { kind: "installed", namespace: from }
      : { kind: "missing", namespace: from };
  }
  const typed = Object.entries(value).some(
    ([k, v]) => k !== "from" && typeof v === "string" && v !== "",
  );
  if (typed) return { kind: "typed" };
  if (canRunOwn) return { kind: "own" };
  return installed.length > 0
    ? { kind: "installed", namespace: installed[0].namespace }
    : { kind: "typed" };
}

export function adoptsDefault(
  value: Connection,
  choice: ConnectionChoice,
): string | null {
  return choice.kind === "installed" && !value.from ? choice.namespace : null;
}

function providersIn(config: Record<string, unknown>): string[] {
  return Object.values(config)
    .map((v) =>
      v && typeof v === "object" && !Array.isArray(v)
        ? (v as Connection).from
        : undefined,
    )
    .filter((from): from is string => typeof from === "string" && from !== "")
    .map((from) => from.replace(/^yolab-/, ""));
}

export function usesOf(app: AppInfo): string[] {
  return [...new Set(providersIn(app.config))].sort();
}

export function usedBy(app: AppInfo, all: AppInfo[]): string[] {
  return all
    .filter(
      (other) =>
        other.instance_name !== app.instance_name &&
        providersIn(other.config).includes(app.instance_name),
    )
    .map((other) => other.instance_name)
    .sort();
}

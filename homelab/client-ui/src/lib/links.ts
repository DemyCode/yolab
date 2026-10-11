import type { AppInfo } from "@/types/apps";
import type { SchemaProp } from "@/lib/schema";

export const FROM = "$from";

export interface LinkCandidate {
  reference: string;
  instance: string;
  member: string | null;
  app_id: string;
}

export type LinkValue = Record<string, unknown>;

export type LinkChoice =
  | { kind: "own" }
  | { kind: "app"; reference: string }
  | { kind: "missing"; reference: string }
  | { kind: "typed" };

export function linkedReference(value: LinkValue): string {
  const chosen = value[FROM];
  if (typeof chosen === "string" && chosen !== "") return chosen;
  const legacy = value.from;
  if (typeof legacy === "string" && legacy.startsWith("yolab-")) {
    return legacy.slice("yolab-".length);
  }
  return "";
}

export function linkChoice(
  value: LinkValue,
  candidates: LinkCandidate[],
  canRunOwn: boolean,
  typing: boolean,
): LinkChoice {
  if (typing) return { kind: "typed" };
  const reference = linkedReference(value);
  if (reference) {
    return candidates.some((c) => c.reference === reference)
      ? { kind: "app", reference }
      : { kind: "missing", reference };
  }
  const typed = Object.entries(value).some(
    ([k, v]) => k !== FROM && k !== "from" && typeof v === "string" && v !== "",
  );
  if (typed) return { kind: "typed" };
  if (canRunOwn) return { kind: "own" };
  return candidates.length > 0
    ? { kind: "app", reference: candidates[0].reference }
    : { kind: "typed" };
}

export function adoptsDefault(
  value: LinkValue,
  choice: LinkChoice,
): string | null {
  return choice.kind === "app" && !linkedReference(value)
    ? choice.reference
    : null;
}

export function linkLabel(c: LinkCandidate): string {
  if (c.member) return `${c.member} in ${c.instance}`;
  return c.instance === c.app_id ? c.instance : `${c.instance} (${c.app_id})`;
}

export function missingLabel(reference: string): string {
  const [instance, member] = reference.split("/");
  return member ? `${member} in ${instance}` : instance;
}

export function linksQuery(prop: SchemaProp): string | null {
  const from = prop["x-yolab-from"] ?? [];
  const outputs =
    prop["x-yolab-outputs"] ??
    Object.keys(prop.properties ?? {}).filter((k) => k !== "from");
  if (outputs.length === 0) return null;
  const query = new URLSearchParams({
    from: from.join(","),
    outputs: outputs.join(","),
  });
  return `/api/links?${query.toString()}`;
}

export function boxesOf(prop: SchemaProp): [string, SchemaProp][] {
  const wanted = prop["x-yolab-outputs"];
  return Object.entries(prop.properties ?? {}).filter(
    ([k]) => k !== "from" && (!wanted || wanted.includes(k)),
  );
}

function instancesIn(config: Record<string, unknown>): string[] {
  return Object.values(config)
    .map((v) =>
      v && typeof v === "object" && !Array.isArray(v)
        ? linkedReference(v as LinkValue)
        : "",
    )
    .filter((reference) => reference !== "")
    .map((reference) => reference.split("/")[0]);
}

export function usesOf(app: AppInfo): string[] {
  return [...new Set(instancesIn(app.config))].sort();
}

export function usedBy(app: AppInfo, all: AppInfo[]): string[] {
  return all
    .filter(
      (other) =>
        other.instance_name !== app.instance_name &&
        instancesIn(other.config).includes(app.instance_name),
    )
    .map((other) => other.instance_name)
    .sort();
}

import type { AppDefinition, AppInfo } from "@/types/apps";

export type InstallMode = "fresh" | "duplicate" | "restore";

export interface InstallOrigin {
  mode: InstallMode;
  fromInstance: string | null;
  namespace: string | null;
  snapshot: string | null;
}

export interface InstallSourcePayload {
  kind: "duplicate" | "backup";
  from_instance?: string;
  namespace?: string;
  snapshot_id?: string;
  with_data: boolean;
}

export function installOrigin(params: URLSearchParams): InstallOrigin {
  const fromInstance = params.get("from");
  const namespace = params.get("restore");
  const snapshot = params.get("snapshot");
  if (fromInstance) {
    return { mode: "duplicate", fromInstance, namespace: null, snapshot: null };
  }
  if (namespace && snapshot) {
    return { mode: "restore", fromInstance: null, namespace, snapshot };
  }
  return { mode: "fresh", fromInstance: null, namespace: null, snapshot: null };
}

export function copiesDataByDefault(mode: InstallMode): boolean {
  return mode === "restore";
}

export function snapshotNamespace(origin: InstallOrigin): string | null {
  return origin.mode === "restore" ? origin.namespace : null;
}

export function installSource(
  origin: InstallOrigin,
  withData: boolean,
  snapshot: string,
): InstallSourcePayload | undefined {
  if (origin.mode === "duplicate") {
    return {
      kind: "duplicate",
      from_instance: origin.fromInstance ?? "",
      with_data: withData,
    };
  }
  if (origin.mode === "restore") {
    return {
      kind: "backup",
      namespace: origin.namespace ?? "",
      snapshot_id: snapshot || (origin.snapshot ?? ""),
      with_data: withData,
    };
  }
  return undefined;
}

export function keepsSourceAddress(mode: InstallMode): boolean {
  return mode === "restore";
}

export function instanceNameFor(
  mode: InstallMode,
  appId: string,
  source: AppDefinition | null,
): string {
  if (mode === "restore" && source?.instance_name) {
    return stripInstanceId(source.instance_name);
  }
  return appId;
}

const INSTANCE_ID = /-[abcdefghijkmnpqrstuvwxyz23456789]{4}$/;

export function stripInstanceId(instanceName: string): string {
  return instanceName.replace(INSTANCE_ID, "");
}

export function addressTakenBy(
  subdomain: string,
  installed: AppInfo[],
): string | null {
  if (!subdomain) return null;
  const clash = installed.find((a) => a.config?.subdomain === subdomain);
  return clash ? clash.instance_name : null;
}

export interface BlockerInput {
  instanceName: string;
  addressTakenBy: string | null;
  requiredMissing: boolean;
  withData: boolean;
  needsBackup: boolean;
  snapshot: string;
  snapshotsLoaded: boolean;
  snapshotCount: number;
}

export function installBlocker(input: BlockerInput): string | null {
  if (!input.instanceName) return "This app has no name to install under.";
  if (input.addressTakenBy) {
    return `That web address already belongs to ${input.addressTakenBy}. Pick another.`;
  }
  if (input.requiredMissing) return "Fill in everything marked required.";
  if (input.withData && input.needsBackup) {
    if (input.snapshotsLoaded && input.snapshotCount === 0) {
      return "There is no backup to copy data from yet.";
    }
    if (!input.snapshot) return "Pick the backup to copy data from.";
  }
  return null;
}

export function phaseFrom(line: string): string | null {
  const trimmed = line.trim();
  if (trimmed.endsWith("…")) return trimmed.slice(0, -1);
  const lower = trimmed.toLowerCase();
  if (lower.includes("pending-install")) return "Installing";
  if (lower.includes("deployed") || lower.startsWith("status:")) {
    return "Almost there";
  }
  return null;
}

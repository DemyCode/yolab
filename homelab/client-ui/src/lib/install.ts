import type { AppDefinition } from "@/types/apps";
import type { RestoreChoices } from "./restore";
import {
  addressField,
  chosenBranchDefaults,
  revealedFields,
  type ConfigSchema,
} from "./schema";

export type InstallMode =
  | "fresh"
  | "duplicate"
  | "restore"
  | "edit"
  | "update";

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
  choices?: RestoreChoices;
}

export function installOrigin(params: URLSearchParams): InstallOrigin {
  const updating = params.get("update");
  if (updating) {
    return {
      mode: "update",
      fromInstance: updating,
      namespace: null,
      snapshot: null,
    };
  }
  const editing = params.get("edit");
  if (editing) {
    return {
      mode: "edit",
      fromInstance: editing,
      namespace: null,
      snapshot: null,
    };
  }
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
  choices?: RestoreChoices,
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
      ...(choices ? { choices } : {}),
    };
  }
  return undefined;
}

export const FILES_FOLDER = "files_folder";
export const FOLDER_SWITCH = "files_folder_existing";

export type FilesFolderCopy = "new" | "shared" | "existing";

function treePath(value: unknown): string {
  return String(value ?? "").replace(/^\/+|\/+$/g, "");
}

function chosenFolder(config: Record<string, unknown>): string {
  return config[FOLDER_SWITCH] === true ? treePath(config[FILES_FOLDER]) : "";
}

export function filesFolderCopy(
  source: Record<string, unknown> | null,
  values: Record<string, unknown>,
): { kind: FilesFolderCopy; from: string; to: string } | null {
  if (!source) return null;
  const offered = (c: Record<string, unknown>) =>
    FILES_FOLDER in c || FOLDER_SWITCH in c;
  if (!offered(source) && !offered(values)) return null;
  const from = chosenFolder(source);
  const to = chosenFolder(values);
  if (from && to === from) return { kind: "shared", from, to };
  if (to) return { kind: "existing", from, to };
  return { kind: "new", from, to };
}

export function keepsSourceAddress(mode: InstallMode): boolean {
  return mode === "restore" || changesLiveApp(mode);
}

export function readsLiveApp(mode: InstallMode): boolean {
  return mode === "duplicate" || changesLiveApp(mode);
}

export function changesLiveApp(mode: InstallMode): boolean {
  return mode === "edit" || mode === "update";
}

export function instanceNameFor(
  mode: InstallMode,
  appId: string,
  source: AppDefinition | null,
): string {
  if (mode === "restore" && source?.instance_name) {
    return stripInstanceId(source.instance_name);
  }
  if (changesLiveApp(mode)) return source?.instance_name ?? "";
  return appId;
}

const INSTANCE_ID = /-[abcdefghijkmnpqrstuvwxyz23456789]{4}$/;

export function stripInstanceId(instanceName: string): string {
  return instanceName.replace(INSTANCE_ID, "");
}

export interface BlockerInput {
  instanceName: string;
  requiredMissing: boolean;
  folderProblem: string | null;
  withData: boolean;
  needsBackup: boolean;
  snapshot: string;
  snapshotsLoaded: boolean;
  snapshotCount: number;
}

export function installBlocker(input: BlockerInput): string | null {
  if (!input.instanceName) return "This service has no name to install under.";
  if (input.requiredMissing) return "Fill in everything marked required.";
  if (input.folderProblem) return input.folderProblem;
  if (input.withData && input.needsBackup) {
    if (input.snapshotsLoaded && input.snapshotCount === 0) {
      return "There is no backup to copy data from yet.";
    }
    if (!input.snapshot) return "Pick the backup to copy data from.";
  }
  return null;
}

export function seedForm(
  schema: ConfigSchema,
  source: Record<string, unknown> | null,
  mode: InstallMode,
  generate: (length: number) => string,
): Record<string, unknown> {
  const seed: Record<string, unknown> = source ? { ...source } : {};
  for (const [name, prop] of Object.entries(schema.properties ?? {})) {
    if (prop.writeOnly) {
      if (prop.generate && seed[name] === undefined) {
        seed[name] = generate(Math.max(24, prop.minLength ?? 0));
      }
    } else if (seed[name] === undefined && prop.default !== undefined) {
      seed[name] = prop.default;
    }
  }
  for (const [name, prop] of revealedFields(schema)) {
    if (prop.writeOnly && prop.generate && seed[name] === undefined) {
      seed[name] = generate(Math.max(24, prop.minLength ?? 0));
    }
  }
  Object.assign(seed, chosenBranchDefaults(schema, seed));
  const address = addressField(schema);
  if (source && address && !keepsSourceAddress(mode)) {
    delete seed[address];
  }
  return seed;
}

import { useSyncExternalStore } from "react";
import { streamEvents, type StreamResult } from "./api";

export type UpdateKind = "update" | "settings";

export type AppUpdate =
  | { kind: UpdateKind; running: true }
  | { kind: UpdateKind; running: false; ok: true; version: string }
  | { kind: UpdateKind; running: false; ok: false; error: string };

const updates = new Map<string, AppUpdate>();
const listeners = new Set<() => void>();

function put(instance: string, update: AppUpdate | null) {
  if (update) updates.set(instance, update);
  else updates.delete(instance);
  for (const listener of listeners) listener();
}

export function settled(
  kind: UpdateKind,
  result: StreamResult,
  version: string,
): AppUpdate {
  return result.ok
    ? { kind, running: false, ok: true, version }
    : { kind, running: false, ok: false, error: result.error };
}

export function failure(result: StreamResult): string | null {
  return result.ok ? null : result.error;
}

export function startUpdate(
  instance: string,
  kind: UpdateKind,
  body?: unknown,
): Promise<string | null> {
  put(instance, { kind, running: true });
  let version = "";
  const init: RequestInit =
    body === undefined
      ? { method: "POST" }
      : { method: "POST", body: JSON.stringify(body) };
  return new Promise((accepted) => {
    void streamEvents(
      `/api/apps/${instance}/update`,
      init,
      (line) => {
        const found = /^Now on version (.+)$/.exec(line);
        if (found) version = found[1];
      },
      () => accepted(null),
    ).then((result) => {
      put(instance, settled(kind, result, version));
      accepted(failure(result));
    });
  });
}

export type UpdateNotice =
  | { tone: "success"; title: string }
  | { tone: "error"; title: string; body: string };

export function updateNotice(
  update: AppUpdate | undefined,
  name: string,
): UpdateNotice | null {
  if (!update || update.running) return null;
  if (!update.ok) {
    return {
      tone: "error",
      title:
        update.kind === "settings"
          ? "The new settings were not applied"
          : "The update did not finish",
      body: update.error,
    };
  }
  if (update.kind === "settings") {
    return { tone: "success", title: `${name} runs with its new settings` };
  }
  return {
    tone: "success",
    title: update.version
      ? `${name} now runs version ${update.version}`
      : `${name} was reinstalled`,
  };
}

export function forgetFinished(instance: string): void {
  if (updates.get(instance)?.running === false) put(instance, null);
}

export function forgetUpdates(): void {
  updates.clear();
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useAppUpdate(instance: string): AppUpdate | undefined {
  return useSyncExternalStore(subscribe, () => updates.get(instance));
}

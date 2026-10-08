export interface ServiceInstance {
  instance: string;
  app_id: string;
  title: string;
  url: string;
}

export type ServiceChoice =
  | { kind: "own" }
  | { kind: "installed"; url: string }
  | { kind: "url"; url: string };

export function serviceChoice(
  value: string,
  installed: ServiceInstance[],
  canRunOwn: boolean,
): ServiceChoice {
  if (value === "") {
    if (canRunOwn) return { kind: "own" };
    return installed.length > 0
      ? { kind: "installed", url: installed[0].url }
      : { kind: "url", url: "" };
  }
  return installed.some((s) => s.url === value)
    ? { kind: "installed", url: value }
    : { kind: "url", url: value };
}

export function serviceLabel(s: ServiceInstance): string {
  return s.instance === s.app_id ? s.instance : `${s.instance} (${s.app_id})`;
}

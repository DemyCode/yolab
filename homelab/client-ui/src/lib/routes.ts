export function fromOldBoxPath(path: string): string {
  const rest = path.replace(/^\/box\/?/, "").replace(/\/$/, "");
  if (rest === "") return "/system";
  if (rest === "system") return "/system/updates";
  return `/system/${rest}`;
}

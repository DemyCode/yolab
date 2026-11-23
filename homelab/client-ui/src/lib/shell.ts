export const CHANGE_ADDRESS_URL = "yolab-shell://change-address";

declare global {
  interface Window {
    __YOLAB_SHELL__?: boolean;
  }
}

export function inYolabShell(
  w: Pick<Window, "__YOLAB_SHELL__"> = window,
): boolean {
  return w.__YOLAB_SHELL__ === true;
}

export function changeServer(
  w: { location: { href: string } } = window,
): void {
  w.location.href = CHANGE_ADDRESS_URL;
}

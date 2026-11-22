import { api, ApiError } from "./api";

export async function switchChannel(
  url: string,
  ref: string,
): Promise<string | null> {
  try {
    await api.put("/api/update/channel", { url: url.trim(), ref: ref.trim() });
    return null;
  } catch (e) {
    return e instanceof ApiError ? e.message : "Could not switch the channel";
  }
}

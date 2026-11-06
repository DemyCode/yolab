import { describe, expect, it } from "vitest";

import { keptReason, removePath } from "./removeData";

describe("removing a service", () => {
  it("asks for its data to go only when the box is ticked", () => {
    expect(removePath("jellyfin", true)).toBe(
      "/api/apps/jellyfin?delete_data=true",
    );
    expect(removePath("jellyfin", false)).toBe("/api/apps/jellyfin");
  });

  it("says who still uses a folder it keeps", () => {
    expect(keptReason({ folder: "media", used_by: ["radarr", "sonarr"] })).toBe(
      "media, used by radarr, sonarr",
    );
  });
});

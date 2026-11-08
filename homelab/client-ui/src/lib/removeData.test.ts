import { describe, expect, it } from "vitest";

import { folderName, removePath, sharedWith, usedBy } from "./removeData";

describe("removing a service", () => {
  it("deletes data others still use only when asked", () => {
    expect(removePath("jellyfin", true)).toBe(
      "/api/apps/jellyfin?delete_shared=true",
    );
    expect(removePath("jellyfin", false)).toBe("/api/apps/jellyfin");
  });

  it("names the whole tree in words instead of an empty path", () => {
    expect(folderName("")).toBe("/ (the whole file tree)");
    expect(folderName("jellyfin/media")).toBe("/jellyfin/media");
  });

  it("says who uses a folder it keeps", () => {
    expect(usedBy({ folder: "media", used_by: ["radarr", "sonarr"] })).toBe(
      "/media, used by radarr, sonarr",
    );
  });

  it("lists every service that still sees the shared data once", () => {
    expect(
      sharedWith([
        { folder: "jellyfin", used_by: ["filebrowser", "sonarr"] },
        { folder: "Movies", used_by: ["filebrowser"] },
      ]),
    ).toEqual(["filebrowser", "sonarr"]);
  });
});

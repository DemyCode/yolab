import { describe, expect, it } from "vitest";
import { folderChoice, folderSize, removalNote, type Folder } from "./folders";

const movies: Folder = {
  name: "movies-tv",
  title: "Movies & TV",
  size: "1024Gi",
  ready: true,
  used_by: [],
};

describe("choosing a space for an app", () => {
  it("an empty value keeps the files inside the app", () => {
    expect(folderChoice("", [movies])).toEqual({ kind: "inside" });
  });

  it("a saved name is shown as the space it names", () => {
    expect(folderChoice("movies-tv", [movies])).toEqual({
      kind: "folder",
      folder: movies,
    });
  });

  it("a space that was removed is shown as missing, not silently replaced", () => {
    expect(folderChoice("photos", [movies])).toEqual({
      kind: "missing",
      name: "photos",
    });
  });
});

describe("how a space's size reads", () => {
  it("reads like a disk size", () => {
    expect(folderSize("1024Gi")).toBe("1 TB");
    expect(folderSize("500Gi")).toBe("537 GB");
    expect(folderSize("odd")).toBe("odd");
  });
});

describe("what removing an app does to its spaces", () => {
  it("names the spaces that go and who keeps the others", () => {
    expect(
      removalNote({
        deleted: ["TV"],
        kept: [{ title: "Movies", used_by: ["jellyfin", "plex"] }],
      }),
    ).toBe(
      "TV is used by no other app, so it is deleted with its files about an hour after it is removed. Movies stays for jellyfin and plex.",
    );
    expect(removalNote({ deleted: ["TV", "Music"], kept: [] })).toBe(
      "TV and Music are used by no other app, so they are deleted with their files about an hour after it is removed.",
    );
  });

  it("says nothing about spaces when the app uses none", () => {
    expect(removalNote({ deleted: [], kept: [] })).toBe("");
  });

  it("still warns before the list has loaded", () => {
    expect(removalNote(null)).toContain("deleted with their files");
  });
});

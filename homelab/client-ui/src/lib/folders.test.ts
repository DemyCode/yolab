import { describe, expect, it } from "vitest";
import { folderChoice, folderSize, usedByLine, type Folder } from "./folders";

const movies: Folder = {
  name: "movies-tv",
  title: "Movies & TV",
  size: "1024Gi",
  ready: true,
  used_by: [],
};

describe("choosing a folder for an app", () => {
  it("an empty value keeps the files inside the app", () => {
    expect(folderChoice("", [movies])).toEqual({ kind: "inside" });
  });

  it("a saved name is shown as the folder it names", () => {
    expect(folderChoice("movies-tv", [movies])).toEqual({
      kind: "folder",
      folder: movies,
    });
  });

  it("a folder that was removed is shown as missing, not silently replaced", () => {
    expect(folderChoice("photos", [movies])).toEqual({
      kind: "missing",
      name: "photos",
    });
  });
});

describe("how a folder is described", () => {
  it("its size reads like a disk size", () => {
    expect(folderSize("1024Gi")).toBe("1 TB");
    expect(folderSize("500Gi")).toBe("537 GB");
    expect(folderSize("odd")).toBe("odd");
  });

  it("names the apps that use it, and only the first few", () => {
    expect(usedByLine(movies)).toBe("No app uses it yet");
    expect(usedByLine({ ...movies, used_by: ["jellyfin", "sonarr"] })).toBe(
      "Used by jellyfin, sonarr",
    );
    expect(usedByLine({ ...movies, used_by: ["a", "b", "c", "d", "e"] })).toBe(
      "Used by a, b and 3 more",
    );
  });
});

import { describe, expect, it } from "vitest";
import { folderChoice, folderSize, type Folder } from "./folders";

const movies: Folder = {
  name: "movies-tv",
  title: "Movies & TV",
  size: "1024Gi",
  ready: true,
  used_by: [],
  kind: "files",
  app: null,
  origin: null,
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

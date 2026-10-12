import { describe, expect, it } from "vitest";
import {
  folderBackupLine,
  folderChoice,
  folderSize,
  isDataFolder,
  sharedFolders,
  usedByLine,
  type Folder,
} from "./folders";

const movies: Folder = {
  name: "movies-tv",
  title: "Movies & TV",
  size: "1024Gi",
  ready: true,
  used_by: [],
  kind: "files",
  app: null,
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

describe("how a folder's backups read", () => {
  it("a folder is saved with the apps that use it", () => {
    expect(folderBackupLine({ ...movies, used_by: ["jellyfin"] })).toBe(
      "Saved with jellyfin's backups",
    );
    expect(
      folderBackupLine({ ...movies, used_by: ["jellyfin", "sonarr"] }),
    ).toBe("Saved with the backups of every app that uses it");
  });

  it("a folder no app uses says it is not backed up, and why", () => {
    expect(folderBackupLine(movies)).toBe("Not backed up: no app uses it");
    const leftOver: Folder = { ...movies, kind: "state", app: "jellyfin" };
    expect(isDataFolder(leftOver)).toBe(true);
    expect(folderBackupLine(leftOver)).toBe(
      "Not backed up: the app it belonged to is gone",
    );
  });
});

describe("the folder picker", () => {
  it("offers only shared folders, while Your files lists app folders too", () => {
    const appData: Folder = {
      ...movies,
      name: "notediscovery-ikcj-data",
      kind: "state",
      app: "notediscovery",
    };
    expect(sharedFolders([movies, appData])).toEqual([movies]);
  });
});

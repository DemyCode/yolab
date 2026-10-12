import { describe, expect, it } from "vitest";
import {
  NO_CHOICES,
  choicesFor,
  dataOptions,
  folderOptions,
  settingValues,
  sharedWarning,
  shownFolders,
  type DataRow,
  type FolderRow,
  type RestorePlan,
} from "./restore";

const movies: FolderRow = {
  name: "movies",
  title: "Movies",
  capacity: "500Gi",
  backed_up: true,
  exists: true,
  used_by: ["sonarr"],
};

const data: DataRow = {
  claim: "jellyfin-data",
  capacity: "50Gi",
  backed_up: true,
  kept: null,
};

const values = (options: { value: string }[]) => options.map((o) => o.value);

describe("what can happen to a folder the backup used", () => {
  it("a folder that is still there is used as it is, or replaced", () => {
    expect(values(folderOptions(movies))).toEqual(["use", "replace"]);
  });

  it("a missing folder comes back from the backup, or starts empty", () => {
    expect(values(folderOptions({ ...movies, exists: false }))).toEqual([
      "restore",
      "empty",
    ]);
  });

  it("a folder that was not saved can only be used or started empty", () => {
    expect(values(folderOptions({ ...movies, backed_up: false }))).toEqual([
      "use",
    ]);
    expect(
      values(folderOptions({ ...movies, backed_up: false, exists: false })),
    ).toEqual(["empty"]);
  });
});

describe("what can happen to the app's own data", () => {
  it("a reinstall uses the data kept after an uninstall unless told to replace it", () => {
    expect(
      values(dataOptions({ ...data, kept: "jellyfin-data" }, "reinstall")),
    ).toEqual(["use", "replace"]);
  });

  it("a reinstall with nothing kept brings the data back from the backup", () => {
    expect(values(dataOptions(data, "reinstall"))).toEqual([
      "restore",
      "empty",
    ]);
  });

  it("restoring a running app replaces its data unless told to keep it", () => {
    expect(values(dataOptions(data, "in-place"))).toEqual(["replace", "use"]);
  });

  it("data that was never saved is left as it is", () => {
    expect(
      values(dataOptions({ ...data, backed_up: false }, "in-place")),
    ).toEqual(["use"]);
  });
});

describe("the choices sent with a restore", () => {
  const plan: RestorePlan = {
    folders: [movies, { ...movies, name: "downloads", exists: false }],
    data: [data],
  };

  it("every row gets its first option unless the person picked another", () => {
    expect(choicesFor(plan, "reinstall", NO_CHOICES, null)).toEqual({
      folders: { movies: "use", downloads: "restore" },
      data: { "jellyfin-data": "restore" },
    });
    expect(
      choicesFor(
        plan,
        "reinstall",
        { folders: { movies: "replace" }, data: { "jellyfin-data": "empty" } },
        null,
      ),
    ).toEqual({
      folders: { movies: "replace", downloads: "restore" },
      data: { "jellyfin-data": "empty" },
    });
  });

  it("a pick that no longer fits the folder falls back to its first option", () => {
    expect(
      choicesFor(
        plan,
        "reinstall",
        { folders: { downloads: "replace" }, data: {} },
        null,
      ).folders.downloads,
    ).toBe("restore");
  });

  it("a folder swapped for another one in the settings is left out", () => {
    expect(shownFolders(plan, ["movies", "films"]).map((f) => f.name)).toEqual([
      "movies",
    ]);
    expect(
      Object.keys(
        choicesFor(plan, "reinstall", NO_CHOICES, ["movies"]).folders,
      ),
    ).toEqual(["movies"]);
  });
});

describe("restoring a space", () => {
  it("names the other apps that would see the change", () => {
    expect(sharedWarning(movies, "jellyfin")).toBe(
      "sonarr uses it too, and would see the change.",
    );
    expect(
      sharedWarning(
        { ...movies, used_by: ["jellyfin", "sonarr", "radarr"] },
        "jellyfin",
      ),
    ).toBe("sonarr, radarr use it too, and would see the change.");
    expect(
      sharedWarning({ ...movies, used_by: ["jellyfin"] }, "jellyfin"),
    ).toBeNull();
  });
});

describe("the folders named in the settings", () => {
  it("are every text the settings hold, however deep", () => {
    expect(
      settingValues({
        media_folder: "movies",
        photos: "",
        nested: { library: "music", list: ["books"] },
        port: 8080,
      }),
    ).toEqual(["movies", "music", "books"]);
  });
});

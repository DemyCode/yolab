import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "./api";
import { uiSchemaFor, type ConfigSchema } from "./schema";
import {
  activeTreeFolders,
  cleanFolder,
  folderAllowed,
  folderIn,
  folderProblem,
  folderTrail,
  isRootFolder,
  joinFolder,
  forgetListings,
  listFolders,
  ROOT_FOLDER,
  shownFolder,
  rankFolders,
  splitFolder,
  type FolderListing,
} from "./treeFolder";

const FOLDER_SCHEMA: ConfigSchema = {
  properties: {
    files_folder_existing: { type: "boolean", default: false },
  },
  dependencies: {
    files_folder_existing: {
      oneOf: [
        { properties: { files_folder_existing: { const: false } } },
        {
          properties: {
            files_folder_existing: { const: true },
            files_folder: {
              type: "string",
              title: "Folder",
              format: "tree-folder",
            },
          },
        },
      ],
    },
  },
};

function tree(folders: Record<string, string[]>) {
  return async (parent: string): Promise<FolderListing> => {
    const found = folders[parent];
    if (!found)
      throw new ApiError(404, `"${parent}" is not a folder in the tree`);
    return { path: parent, folders: found, truncated: false };
  };
}

describe("splitting what was typed", () => {
  it("keeps the folder being typed apart from the one it is in", () => {
    expect(splitFolder("immich-83uz/up")).toEqual({
      parent: "immich-83uz",
      name: "up",
    });
    expect(splitFolder("imm")).toEqual({ parent: "", name: "imm" });
    expect(splitFolder("immich-83uz/")).toEqual({
      parent: "immich-83uz",
      name: "",
    });
  });

  it("ignores leading and doubled slashes", () => {
    expect(splitFolder("//a//b")).toEqual({ parent: "a", name: "b" });
    expect(cleanFolder("/a//b/")).toBe("a/b");
  });

  it("joins a folder back onto the one it is in", () => {
    expect(joinFolder("", "immich-83uz")).toBe("immich-83uz");
    expect(joinFolder("immich-83uz", "uploads")).toBe("immich-83uz/uploads");
  });
});

describe("suggesting folders", () => {
  const folders = ["immich-83uz", "immich-83uz.files", "jellyfin", "Movies"];

  it("puts the folders that start with what was typed first", () => {
    expect(rankFolders(folders, "imm")).toEqual([
      "immich-83uz",
      "immich-83uz.files",
    ]);
  });

  it("finds a folder from letters inside it, whatever their case", () => {
    expect(rankFolders(folders, "mov")).toEqual(["Movies"]);
    expect(rankFolders(folders, "files")).toEqual(["immich-83uz.files"]);
    expect(rankFolders(folders, "jlf")).toEqual(["jellyfin"]);
  });

  it("shows every folder before anything is typed, up to the limit", () => {
    expect(rankFolders(folders, "", 2)).toHaveLength(2);
    expect(rankFolders(folders, "zzz")).toEqual([]);
  });

  it("only counts a folder that is really there", () => {
    expect(folderIn(folders, "jellyfin")).toBe(true);
    expect(folderIn(folders, "jelly")).toBe(false);
    expect(folderIn(folders, "")).toBe(false);
  });
});

describe("the folder fields of a form", () => {
  it("are only checked while their switch is on", () => {
    expect(
      activeTreeFolders(FOLDER_SCHEMA, {
        files_folder_existing: false,
        files_folder: "x",
      }),
    ).toEqual([]);
    expect(
      activeTreeFolders(FOLDER_SCHEMA, {
        files_folder_existing: true,
        files_folder: "/immich-83uz/uploads/",
      }),
    ).toEqual([
      { name: "files_folder", title: "Folder", value: "immich-83uz/uploads" },
    ]);
  });

  it("are drawn with the folder picker", () => {
    const ui = uiSchemaFor(FOLDER_SCHEMA, "example.yolab.io");
    expect(ui.files_folder).toMatchObject({ "ui:widget": "TreeFolderWidget" });
  });
});

describe("checking the chosen folder", () => {
  const list = tree({ "": ["immich-83uz"], "immich-83uz": ["uploads"] });

  it("lets an existing folder through", async () => {
    expect(
      await folderProblem(
        [{ title: "Folder", value: "immich-83uz/uploads" }],
        list,
      ),
    ).toBeNull();
  });

  it("refuses a folder that is not there", async () => {
    expect(
      await folderProblem(
        [{ title: "Folder", value: "immich-83uz/photos" }],
        list,
      ),
    ).toMatch(/no folder immich-83uz\/photos/);
    expect(
      await folderProblem([{ title: "Folder", value: "gone/uploads" }], list),
    ).toMatch(/no folder gone\/uploads/);
  });

  it("asks for a folder while none is picked", async () => {
    expect(await folderProblem([{ title: "Folder", value: "" }], list)).toBe(
      "Pick the folder.",
    );
  });

  it("says so when the tree cannot be read rather than letting it through", async () => {
    const down = async (): Promise<FolderListing> => {
      throw new ApiError(503, "the file tree did not answer in time");
    };
    expect(
      await folderProblem([{ title: "Folder", value: "immich-83uz" }], down),
    ).toMatch(/could not be read: the file tree did not answer/);
  });
});

describe("the whole file tree", () => {
  it("is picked as /, however many slashes were sent", () => {
    expect(isRootFolder("/")).toBe(true);
    expect(isRootFolder("//")).toBe(true);
    expect(isRootFolder("")).toBe(false);
    expect(isRootFolder("media")).toBe(false);
  });

  it("is a folder that is always there", async () => {
    const down = async (): Promise<FolderListing> => {
      throw new ApiError(503, "the file tree did not answer in time");
    };
    expect(
      await folderProblem([{ title: "Folder", value: ROOT_FOLDER }], down),
    ).toBeNull();
  });

  it("stays / in the form instead of turning into nothing picked", () => {
    expect(
      activeTreeFolders(FOLDER_SCHEMA, {
        files_folder_existing: true,
        files_folder: "/",
      }),
    ).toEqual([{ name: "files_folder", title: "Folder", value: "/" }]);
  });
});

describe("browsing the file tree", () => {
  it("shows every folder from the top of the tree", () => {
    expect(shownFolder("")).toBe("/");
    expect(shownFolder("ok/a")).toBe("/ok/a");
    expect(shownFolder("/ok/")).toBe("/ok");
  });

  it("can go back up to any folder above the one being looked in", () => {
    expect(folderTrail("")).toEqual([{ name: "/", path: "" }]);
    expect(folderTrail("ok/a")).toEqual([
      { name: "/", path: "" },
      { name: "ok", path: "ok" },
      { name: "a", path: "ok/a" },
    ]);
  });

  it("lists every folder of a level, not only the first few", () => {
    const many = Array.from({ length: 20 }, (_, i) => `f${i}`);
    expect(rankFolders(many, "", many.length)).toHaveLength(20);
  });

  it("accepts a folder picked with a leading slash", async () => {
    const list = tree({ "": ["ok"], ok: ["a", "b", "c"] });
    expect(
      await folderProblem([{ title: "Folder", value: "/ok/a" }], list),
    ).toBeNull();
  });
});

describe("what the chart accepts", () => {
  const OLD =
    "^$|^/?[A-Za-z0-9_ -][A-Za-z0-9._ -]*(/[A-Za-z0-9_ -][A-Za-z0-9._ -]*)*/?$";
  const NEW = `^/$|${OLD}`;

  it("never offers / to a chart version that refuses it", () => {
    expect(folderAllowed(OLD, ROOT_FOLDER)).toBe(false);
    expect(folderAllowed(OLD, "/ok/a")).toBe(true);
  });

  it("offers / to a chart that takes it", () => {
    expect(folderAllowed(NEW, ROOT_FOLDER)).toBe(true);
  });

  it("offers any folder when the chart sets no pattern", () => {
    expect(folderAllowed(undefined, ROOT_FOLDER)).toBe(true);
  });
});

describe("tree folder edge cases", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("a missing value is no folder at all", () => {
    expect(cleanFolder(undefined)).toBe("");
    expect(splitFolder(null)).toEqual({ parent: "", name: "" });
    expect(isRootFolder(undefined)).toBe(false);
  });

  it("an exact match ranks before one that only starts the same", () => {
    expect(rankFolders(["photos-old", "photos"], "photos")).toEqual([
      "photos",
      "photos-old",
    ]);
  });

  it("asks again once the listings are forgotten", async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({ path: "", folders: [], truncated: false }),
        ),
    );
    vi.stubGlobal("fetch", fetchMock);
    await listFolders("forget-test");
    forgetListings();
    await listFolders("forget-test");
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it("lists a folder once for a while, and asks again after a failure", async () => {
    let now = 1_000_000;
    vi.spyOn(Date, "now").mockImplementation(() => now);
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({ path: "a", folders: ["b"], truncated: false }),
          { status: 200 },
        ),
    );
    vi.stubGlobal("fetch", fetchMock);
    const first = await listFolders("/cache-test/");
    expect(first.folders).toEqual(["b"]);
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/tree/folders?path=cache-test",
      expect.anything(),
    );
    await listFolders("cache-test");
    expect(fetchMock).toHaveBeenCalledTimes(1);
    now += 15_000;
    await listFolders("cache-test");
    expect(fetchMock).toHaveBeenCalledTimes(2);

    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("down", { status: 500 })),
    );
    await expect(listFolders("cache-fail")).rejects.toThrow("down");
    vi.stubGlobal("fetch", fetchMock);
    await listFolders("cache-fail");
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });

  it("a folder field without a title is named after its key", () => {
    expect(
      activeTreeFolders(
        { properties: { media: { type: "string", format: "tree-folder" } } },
        { media: "/films/" },
      ),
    ).toEqual([{ name: "media", title: "media", value: "films" }]);
    expect(activeTreeFolders({}, {})).toEqual([]);
  });

  it("checks folders against the real tree by default", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              path: "",
              folders: ["default-check"],
              truncated: false,
            }),
            { status: 200 },
          ),
      ),
    );
    await expect(
      folderProblem([{ title: "Media", value: "default-check" }]),
    ).resolves.toBeNull();
  });

  it("a listing that fails with something other than an error still explains", async () => {
    await expect(
      folderProblem([{ title: "Media", value: "a/b" }], () =>
        Promise.reject("offline"),
      ),
    ).resolves.toBe("The file tree could not be read: offline");
  });

  it("a pattern the browser cannot read allows any folder and says so", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    expect(folderAllowed("(", "x")).toBe(true);
    expect(warn).toHaveBeenCalledOnce();
  });
});

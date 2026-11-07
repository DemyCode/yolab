import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import { uiSchemaFor, type ConfigSchema } from "./schema";
import {
  activeTreeFolders,
  cleanFolder,
  folderIn,
  folderProblem,
  folderTrail,
  isRootFolder,
  joinFolder,
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
      throw new ApiError(503, "the file tree did not answer within 20s");
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
      throw new ApiError(503, "the file tree did not answer within 20s");
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

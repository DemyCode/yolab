import { describe, expect, it } from "vitest";
import type { AppInfo } from "@/types/apps";
import {
  arrangeHome,
  groupNameFor,
  installedByGroup,
  memberRows,
  settingUp,
  type GroupRecord,
} from "./groups";

function app(
  instance_name: string,
  app_id: string,
  group?: AppInfo["group"],
): AppInfo {
  return {
    app_id,
    instance_name,
    chart_version: "1.0.0",
    status: "running",
    detail: "",
    outputs: [],
    config: {},
    backup: { enabled: false, schedule: "", last_ok_at: null, running: false },
    group,
  };
}

const movies = { name: "movies-tv", title: "Movies & TV" };

describe("the home screen", () => {
  it("shows a group as its main app with the rest behind the scenes", () => {
    const { loose, groups } = arrangeHome([
      app("notes", "memos"),
      app("sonarr", "sonarr", { ...movies, main: false }),
      app("jellyfin", "jellyfin", { ...movies, main: true }),
    ]);
    expect(loose.map((a) => a.instance_name)).toEqual(["notes"]);
    expect(groups).toHaveLength(1);
    expect(groups[0].title).toBe("Movies & TV");
    expect(groups[0].main.map((a) => a.instance_name)).toEqual(["jellyfin"]);
    expect(groups[0].others.map((a) => a.instance_name)).toEqual(["sonarr"]);
  });

  it("a group whose main app was removed still shows one app up front", () => {
    const { groups } = arrangeHome([
      app("sonarr", "sonarr", { ...movies, main: false }),
      app("radarr", "radarr", { ...movies, main: false }),
    ]);
    expect(groups[0].main).toHaveLength(1);
    expect(groups[0].others).toHaveLength(1);
  });
});

const record: GroupRecord = {
  name: "movies-tv",
  title: "Movies & TV",
  chart: "movies-tv",
  repo: "official",
  version: "0.1.0",
  values: {},
  members: {
    sonarr: "yolab-sonarr-ab12",
    jellyfin: "yolab-jellyfin-cd34",
  },
  status: {
    sonarr: { state: "working" },
    jellyfin: { state: "failed", message: "no space" },
  },
  left: [],
  reused: [],
};

describe("a group's page", () => {
  it("lists each app with the name it was installed under and how it went", () => {
    expect(memberRows(record)).toEqual([
      {
        key: "jellyfin",
        instance: "jellyfin-cd34",
        state: "failed",
        message: "no space",
      },
      { key: "sonarr", instance: "sonarr-ab12", state: "working", message: "" },
    ]);
  });

  it("is setting up while any app waits or works", () => {
    expect(settingUp(record)).toBe(true);
    expect(
      settingUp({
        ...record,
        status: { sonarr: { state: "done" }, jellyfin: { state: "failed" } },
      }),
    ).toBe(false);
  });

  it("a second copy of a group gets the next free name", () => {
    expect(groupNameFor("movies-tv", [])).toBe("movies-tv");
    expect(groupNameFor("movies-tv", ["movies-tv", "movies-tv-2"])).toBe(
      "movies-tv-3",
    );
  });
});

describe("removing a group with its apps", () => {
  it("removes only the apps the group installed, never ones it reused", () => {
    expect(installedByGroup({ ...record, reused: ["jellyfin"] })).toEqual([
      "sonarr-ab12",
    ]);
  });
});

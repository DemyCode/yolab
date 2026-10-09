import { describe, expect, it } from "vitest";
import type { AppInfo } from "@/types/apps";
import type { Folder } from "./folders";
import {
  arrangeHome,
  defaultPlan,
  installOrder,
  setupConfig,
  usedFolders,
  type Setup,
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

const setup: Setup = {
  title: "Movies & TV",
  main: "jellyfin",
  folders: { media: { title: "Movies & TV" } },
  apps: {
    sonarr: { chart: "sonarr", folders: { media_folder: "media" } },
    jellyfin: {
      chart: "jellyfin",
      settings: { hardware_transcoding: true },
      folders: { media_folder: "media" },
    },
    prowlarr: { chart: "prowlarr" },
  },
};

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

describe("installing a setup", () => {
  const folder: Folder = {
    name: "movies",
    title: "Movies & TV",
    size: "1024Gi",
    ready: true,
    used_by: [],
  };

  it("reuses a folder with the same title and an app installed once", () => {
    const plan = defaultPlan(setup, [app("sonarr", "sonarr")], [folder]);
    expect(plan.folders.media).toEqual({ kind: "existing", name: "movies" });
    expect(plan.apps.sonarr).toEqual({ kind: "existing", instance: "sonarr" });
    expect(plan.apps.jellyfin).toEqual({ kind: "new" });
  });

  it("creates what is missing and never guesses between two installed copies", () => {
    const plan = defaultPlan(
      setup,
      [app("sonarr", "sonarr"), app("sonarr-x2k4", "sonarr")],
      [],
    );
    expect(plan.folders.media).toEqual({ kind: "new", title: "Movies & TV" });
    expect(plan.apps.sonarr).toEqual({ kind: "new" });
  });

  it("an app being removed is not offered for reuse", () => {
    const leaving = {
      ...app("sonarr", "sonarr"),
      status: "uninstalling" as const,
    };
    expect(defaultPlan(setup, [leaving], []).apps.sonarr).toEqual({
      kind: "new",
    });
  });

  it("an app's settings and folders go on top of its form defaults", () => {
    expect(
      setupConfig(
        { subdomain: "jellyfin", hardware_transcoding: false },
        setup.apps.jellyfin,
        { media: "movies" },
      ),
    ).toEqual({
      subdomain: "jellyfin",
      hardware_transcoding: true,
      media_folder: "movies",
    });
  });

  it("only folders that a new app uses are created", () => {
    const plan = defaultPlan(setup, [], []);
    expect(usedFolders(setup, plan)).toEqual(new Set(["media"]));
    plan.apps.sonarr = { kind: "skip" };
    plan.apps.jellyfin = { kind: "existing", instance: "jellyfin" };
    expect(usedFolders(setup, plan)).toEqual(new Set());
  });

  it("the main app is installed first", () => {
    expect(installOrder(setup)[0]).toBe("jellyfin");
    expect(installOrder({ ...setup, main: undefined })).toEqual([
      "sonarr",
      "jellyfin",
      "prowlarr",
    ]);
  });
});

describe("a setup that connects its own apps", () => {
  const ai: Setup = {
    title: "AI chat",
    main: "open-webui",
    apps: {
      "open-webui": { chart: "open-webui", uses: { ollama: "ollama" } },
      ollama: { chart: "ollama" },
    },
  };

  it("installs what an app uses before the app, even the main one", () => {
    expect(installOrder(ai)).toEqual(["ollama", "open-webui"]);
  });

  it("points the app at the instance its partner really got", () => {
    expect(
      setupConfig({}, ai.apps["open-webui"], {}, { ollama: "ollama-k3m9" }),
    ).toEqual({ ollama: { from: "yolab-ollama-k3m9" } });
  });

  it("leaves the connection to the form when the partner was left out", () => {
    expect(setupConfig({ ollama: {} }, ai.apps["open-webui"], {}, {})).toEqual(
      { ollama: {} },
    );
  });

  it("an app that uses itself or a loop does not hang the order", () => {
    const loop: Setup = {
      title: "x",
      apps: {
        a: { chart: "a", uses: { x: "b" } },
        b: { chart: "b", uses: { y: "a" } },
      },
    };
    expect(installOrder(loop).sort()).toEqual(["a", "b"]);
  });
});

import { describe, expect, it } from "vitest";
import {
  accessRows,
  appLinks,
  outputState,
  stillWaiting,
  appState,
  appLabel,
  appDisplayName,
  installedByChart,
  nextInstanceName,
  newerVersion,
  availableActions,
  appStatus,
  latestRestore,
  RESTORE_DONE_SHOWN_MS,
  SLOW_START_MS,
  podStatus,
  podProblem,
  waitNote,
  type RestoreRecord,
} from "./apps";
import type { AppInfo, AppOutput, CatalogApp } from "@/types/apps";

function app(over: Partial<AppInfo> = {}): AppInfo {
  return {
    app_id: "gitea",
    instance_name: "gitea",
    chart_version: "1.0.0",
    status: "running",
    detail: "",
    outputs: [],
    config: {},
    backup: { enabled: false, schedule: "", last_ok_at: null, running: false },
    ...over,
  };
}

const out = (
  key: string,
  value: string,
  format: AppOutput["format"] = "text",
  title = "",
): AppOutput => ({
  key,
  title,
  format,
  value: value || null,
  found_at: null,
  from_config: false,
});

describe("installedByChart", () => {
  it("counts every copy, not just the first", () => {
    const counts = installedByChart([
      app({ app_id: "immich", instance_name: "immich" }),
      app({ app_id: "immich", instance_name: "immich-2" }),
      app({ app_id: "gitea", instance_name: "gitea" }),
    ]);
    expect(counts.get("immich")).toBe(2);
    expect(counts.get("gitea")).toBe(1);
    expect(counts.get("never-installed")).toBeUndefined();
  });

  it("copes with the list not having loaded yet", () => {
    expect(installedByChart(undefined).size).toBe(0);
  });
});

describe("appLinks", () => {
  it("returns every address an app publishes, not just the first", () => {
    const links = appLinks(
      app({
        outputs: [
          out("web", "https://a.example", "uri", "Open"),
          out("admin", "https://a.example/admin", "uri", "Admin"),
        ],
      }),
      "example",
    );
    expect(links).toEqual([
      { label: "Open", url: "https://a.example" },
      { label: "Admin", url: "https://a.example/admin" },
    ]);
  });

  it("falls back to a label a person can read", () => {
    const [link] = appLinks(
      app({ outputs: [out("web", "https://a.example", "uri")] }),
      "example",
    );
    expect(link.label).toBe("Open");
  });

  it("offers the derived address before any output has been scraped", () => {
    expect(
      appLinks(app({ config: { subdomain: "git" } }), "box.yolab.io"),
    ).toEqual([{ label: "Open", url: "https://git.box.yolab.io" }]);
  });

  it("offers no YoLab address when the app has it switched off", () => {
    expect(
      appLinks(
        app({ config: { subdomain: "git", yolab_enabled: false } }),
        "box.yolab.io",
      ),
    ).toEqual([]);
  });

  it("does not offer the same address twice under two labels", () => {
    const links = appLinks(
      app({
        config: { subdomain: "git" },
        outputs: [out("web", "https://git.box.yolab.io/", "uri", "Open")],
      }),
      "box.yolab.io",
    );
    expect(links).toHaveLength(1);
  });

  it("ignores outputs that are not addresses", () => {
    const links = appLinks(
      app({ outputs: [out("password", "hunter2"), out("empty", "", "uri")] }),
      "",
    );
    expect(links).toEqual([]);
  });

  it("offers nothing derived when there is no tunnel domain yet", () => {
    expect(appLinks(app({ config: { subdomain: "git" } }), "")).toEqual([]);
  });
});

describe("accessRows", () => {
  it("shows everything an app reports except the addresses already offered as links", () => {
    const rows = accessRows([
      out("web", "https://a.example", "uri"),
      out("password", "hunter2", "secret"),
      out("onion", "", "uri"),
    ]);
    expect(rows.map((r) => r.key)).toEqual(["password", "onion"]);
  });

  it("keeps the order the chart declared", () => {
    const rows = accessRows([
      out("b", "2"),
      out("a", "1"),
      out("c", "", "multiline"),
    ]);
    expect(rows.map((r) => r.key)).toEqual(["b", "a", "c"]);
  });
});

describe("outputState", () => {
  it("is ready once a value is known", () => {
    expect(outputState(out("password", "hunter2", "secret"))).toBe("ready");
  });

  it("is waiting while the app has not printed it yet", () => {
    expect(outputState(out("onion", ""))).toBe("waiting");
  });

  it("is unset, not waiting, when it comes from a setting left empty", () => {
    expect(outputState({ ...out("pin", ""), from_config: true })).toBe("unset");
  });
});

describe("stillWaiting", () => {
  it("is true while any output is still expected from the app", () => {
    expect(stillWaiting([out("a", "1"), out("b", "")])).toBe(true);
  });

  it("is false once everything is known or comes from settings", () => {
    expect(
      stillWaiting([out("a", "1"), { ...out("pin", ""), from_config: true }]),
    ).toBe(false);
  });
});

describe("nextInstanceName", () => {
  it("uses the plain name when nothing has taken it", () => {
    expect(nextInstanceName("gitea", [])).toBe("gitea");
  });

  it("numbers the second copy rather than colliding", () => {
    expect(nextInstanceName("gitea", [app({ instance_name: "gitea" })])).toBe(
      "gitea-2",
    );
  });

  it("skips over names already taken further up the run", () => {
    const installed = [
      app({ instance_name: "gitea" }),
      app({ instance_name: "gitea-2" }),
      app({ instance_name: "gitea-3" }),
    ];
    expect(nextInstanceName("gitea", installed)).toBe("gitea-4");
  });

  it("still returns something unique when the run is exhausted", () => {
    const installed = [
      app({ instance_name: "gitea" }),
      ...Array.from({ length: 98 }, (_, i) =>
        app({ instance_name: `gitea-${i + 2}` }),
      ),
    ];
    const name = nextInstanceName("gitea", installed);
    expect(installed.some((a) => a.instance_name === name)).toBe(false);
  });
});

describe("appState", () => {
  it("distinguishes the two states a tile must not confuse", () => {
    expect(appState(app({ status: "uninstalling" }))).toBe("removing");
    expect(appState(app({ status: "starting" }))).toBe("starting");
    expect(appState(app({ status: "running" }))).toBe("ready");
  });

  it("treats a data copy as its own state, not as starting", () => {
    expect(appState(app({ status: "copying" }))).toBe("copying");
  });

  it("keeps a failed install as its own state instead of calling it ready", () => {
    expect(appState(app({ status: "failed" }))).toBe("failed");
  });

  it("labels a failed install plainly on its tile, keeping the reason for its page", () => {
    const failed = app({
      status: "failed",
      detail: "gitea could not be installed — the log above is helm's own",
    });
    expect(appLabel(failed, appState(failed))).toBe("Failed installation");
  });

  it("keeps an app that ran and then broke apart from a failed install", () => {
    const stopped = app({
      status: "stopped",
      detail: "app: panic: config.yaml: no such file",
    });
    expect(appState(stopped)).toBe("stopped");
    expect(appLabel(stopped, appState(stopped))).toBe("Stopped working");
  });

  it("shows the server's copy progress instead of a generic label", () => {
    const copying = app({
      status: "copying",
      detail: "Copying this app's files… 37%",
    });
    expect(appLabel(copying, appState(copying))).toBe(
      "Copying this app's files… 37%",
    );
  });
});

describe("appDisplayName", () => {
  const catalog = [
    { id: "gitea", name: "Gitea" } as unknown as CatalogApp,
    { id: "immich", name: "Immich" } as unknown as CatalogApp,
  ];

  it("shows the catalog name when you have exactly one", () => {
    const only = app({ instance_name: "gitea-ab23", instance_id: "ab23" });
    expect(appDisplayName(only, catalog, [only])).toBe("Gitea");
  });

  it("tells two copies apart by their web address, since that is what differs", () => {
    const work = app({
      instance_name: "gitea-ab23",
      instance_id: "ab23",
      config: { subdomain: "git-work" },
    });
    const home = app({
      instance_name: "gitea-cd34",
      instance_id: "cd34",
      config: { subdomain: "git-home" },
    });
    expect(appDisplayName(work, catalog, [work, home])).toBe(
      "Gitea (git-work)",
    );
    expect(appDisplayName(home, catalog, [work, home])).toBe(
      "Gitea (git-home)",
    );
  });

  it("does not disambiguate against a different chart", () => {
    const gitea = app({ instance_name: "gitea-ab23", instance_id: "ab23" });
    const immich = app({
      app_id: "immich",
      instance_name: "immich-cd34",
      instance_id: "cd34",
    });
    expect(appDisplayName(gitea, catalog, [gitea, immich])).toBe("Gitea");
  });

  it("falls back to the generated id when a copy publishes no address", () => {
    const a = app({ instance_name: "gitea-ab23", instance_id: "ab23" });
    const b = app({ instance_name: "gitea-cd34", instance_id: "cd34" });
    expect(appDisplayName(a, catalog, [a, b])).toBe("Gitea (ab23)");
  });

  it("still honours a name chosen before names were dropped", () => {
    const named = app({ instance_name: "my-code-x7k2", instance_id: "x7k2" });
    expect(appDisplayName(named, catalog, [named])).toBe("my-code");
  });

  it("copes with a chart that is no longer in the catalog", () => {
    const gone = app({ instance_name: "gitea-ab23", instance_id: "ab23" });
    expect(appDisplayName(gone, [], [gone])).toBe("gitea");
  });
});

describe("newerVersion", () => {
  const listed = (chart_version: string) =>
    ({ id: "gitea", chart_version }) as unknown as CatalogApp;
  const running = (chart_version: string) => app({ chart_version });

  it("names the catalog's version when it differs from the installed one", () => {
    expect(newerVersion(running("0.1.6"), listed("0.1.7"))).toBe("0.1.7");
  });

  it("is quiet when the app already runs the catalog's version", () => {
    expect(newerVersion(running("0.1.7"), listed("0.1.7"))).toBeNull();
  });

  it("is quiet when either version is unknown", () => {
    expect(newerVersion(running(""), listed("0.1.7"))).toBeNull();
    expect(newerVersion(running("0.1.6"), undefined)).toBeNull();
  });
});

describe("availableActions", () => {
  it("offers a failed install a retry, new settings and removal", () => {
    expect([...availableActions("failed", false)].sort()).toEqual([
      "remove",
      "retry",
      "settings",
    ]);
  });

  it("lets settings change in every state but removal", () => {
    for (const state of [
      "starting",
      "copying",
      "stopped",
      "ready",
      "failed",
    ] as const) {
      expect(availableActions(state, false).has("settings")).toBe(true);
    }
    expect(availableActions("removing", false).has("settings")).toBe(false);
  });

  it("offers nothing while the app is being removed or restored", () => {
    expect(availableActions("removing", false).size).toBe(0);
    expect(availableActions("ready", true).size).toBe(0);
  });

  it("does not offer to back up an app that is not up yet", () => {
    for (const state of ["starting", "copying"] as const) {
      const actions = availableActions(state, false);
      expect(actions.has("backup")).toBe(false);
      expect(actions.has("remove")).toBe(true);
    }
  });

  it("offers a running app everything but a retry", () => {
    const actions = availableActions("ready", false);
    expect(actions.has("retry")).toBe(false);
    expect(actions.has("backup")).toBe(true);
    expect(actions.has("update")).toBe(true);
  });
});

describe("appStatus", () => {
  it("gives each state one tone, with red kept for what needs the user", () => {
    expect(appStatus("ready").tone).toBe("live");
    expect(appStatus("starting").tone).toBe("busy");
    expect(appStatus("stopped").tone).toBe("warn");
    expect(appStatus("failed").tone).toBe("error");
  });
});

describe("latestRestore", () => {
  const now = Date.parse("2026-10-01T12:00:00Z");
  const rec = (over: Partial<RestoreRecord>): RestoreRecord => ({
    id: "r",
    namespace: "yolab-gitea",
    started_at: "2026-10-01T11:00:00Z",
    finished_at: null,
    state: "running",
    ...over,
  });

  it("follows the newest restore of this app, not an older one", () => {
    const old = rec({
      id: "old",
      state: "failed",
      started_at: "2026-09-01T00:00:00Z",
      finished_at: "2026-09-01T00:05:00Z",
    });
    const current = rec({ id: "new" });
    expect(latestRestore([old, current], "yolab-gitea", now)?.id).toBe("new");
  });

  it("ignores other apps' restores", () => {
    expect(
      latestRestore([rec({ namespace: "yolab-x" })], "yolab-gitea", now),
    ).toBeNull();
  });

  it("shows a finished restore for a while, then lets it go", () => {
    const recent = rec({
      state: "succeeded",
      finished_at: new Date(now - 60_000).toISOString(),
    });
    const stale = rec({
      state: "failed",
      finished_at: new Date(now - RESTORE_DONE_SHOWN_MS - 1).toISOString(),
    });
    expect(latestRestore([recent], "yolab-gitea", now)?.state).toBe(
      "succeeded",
    );
    expect(latestRestore([stale], "yolab-gitea", now)).toBeNull();
  });
});

describe("waitNote", () => {
  const now = Date.parse("2026-10-01T10:00:00Z");
  const ago = (ms: number) => new Date(now - ms).toISOString();
  const ahead = (ms: number) => new Date(now + ms).toISOString();

  it("says how long an app has been starting", () => {
    const a = app({ status: "starting", since: ago(3 * 60_000) });
    expect(waitNote(a, "starting", now)).toBe("Started 3 minutes ago.");
  });

  it("says a start is taking longer than usual once it is", () => {
    const a = app({
      status: "starting",
      since: ago(SLOW_START_MS + 4 * 60_000),
    });
    expect(waitNote(a, "starting", now)).toMatch(
      /^It has been 14 minutes, longer than usual/,
    );
  });

  it("does not call a long copy slow, big copies take time", () => {
    const a = app({ status: "copying", since: ago(SLOW_START_MS * 3) });
    expect(waitNote(a, "copying", now)).toBe("Started 30 minutes ago.");
  });

  it("counts down to the next try of an app that keeps stopping", () => {
    const a = app({ status: "stopped", retry_at: ahead(150_000) });
    expect(waitNote(a, "stopped", now)).toBe("Next try in about 3 minutes.");
    const soon = app({ status: "stopped", retry_at: ahead(20_000) });
    expect(waitNote(soon, "stopped", now)).toBe("Next try in a few seconds.");
  });

  it("says it is trying again once the next try is due", () => {
    const a = app({ status: "failed", retry_at: ago(30_000) });
    expect(waitNote(a, "failed", now)).toBe("Trying to start it again now…");
  });

  it("stays quiet when there is no time to tell", () => {
    expect(waitNote(app({ status: "starting" }), "starting", now)).toBeNull();
    expect(waitNote(app({ status: "stopped" }), "stopped", now)).toBeNull();
    expect(waitNote(app({ since: ago(60_000) }), "ready", now)).toBeNull();
    expect(
      waitNote(app({ status: "starting", since: "soon" }), "starting", now),
    ).toBeNull();
  });
});

describe("podProblem", () => {
  const container = (name: string, state: string, init = false) => ({
    name,
    init,
    ready: state === "running",
    state,
    restarts: 0,
  });

  it("names the part that holds a pod back", () => {
    expect(
      podProblem({
        name: "gateway",
        phase: "Pending",
        ready: false,
        containers: [
          container("wg-register", "Completed", true),
          container("caddy", "PodInitializing"),
          container("ollama", "ImagePullBackOff"),
        ],
      }),
    ).toBe("ollama: ImagePullBackOff");
  });

  it("says nothing for a ready pod or one that is only waiting its turn", () => {
    expect(
      podProblem({ name: "p", phase: "Running", ready: true, containers: [] }),
    ).toBeNull();
    expect(
      podProblem({
        name: "p",
        phase: "Pending",
        ready: false,
        containers: [container("app", "PodInitializing")],
      }),
    ).toBeNull();
  });
});

describe("podStatus", () => {
  it("puts every Kubernetes phase into plain words", () => {
    const pod = (phase: string, ready = false) => ({ name: "p", phase, ready });
    expect(podStatus(pod("Running", true))).toBe("Running");
    expect(podStatus(pod("Running"))).toBe("Starting");
    expect(podStatus(pod("Pending"))).toBe("Getting ready");
    expect(podStatus(pod("Succeeded"))).toBe("Finished its job");
    expect(podStatus(pod("Failed"))).toBe("Stopped");
    expect(podStatus(pod("Unknown"))).toBe("Not responding");
  });
});

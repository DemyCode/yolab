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
} from "./apps";
import type { AppInfo, AppOutput, CatalogApp } from "@/types/apps";

function app(over: Partial<AppInfo> = {}): AppInfo {
  return {
    app_id: "gitea",
    instance_name: "gitea",
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

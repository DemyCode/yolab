import { describe, expect, it } from "vitest";
import {
  appFactRows,
  appLinks,
  appState,
  installedByChart,
  nextInstanceName,
} from "./apps";
import type { AppInfo, AppOutput, OutputSpec } from "@/types/apps";

function app(over: Partial<AppInfo> = {}): AppInfo {
  return {
    app_id: "gitea",
    instance_name: "gitea",
    status: "running",
    detail: "",
    outputs: [],
    outputs_spec: [],
    config: {},
    backup: { enabled: false, schedule: "", last_ok_at: null, running: false },
    ...over,
  };
}

const out = (
  key: string,
  value: string,
  type: AppOutput["type"] = "text",
  label = "",
): AppOutput => ({ key, label, value, type });

const spec = (
  key: string,
  label: string,
  type: OutputSpec["type"] = "text",
): OutputSpec => ({ key, label, type });

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
          out("web", "https://a.example", "url", "Open"),
          out("admin", "https://a.example/admin", "url", "Admin"),
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
      app({ outputs: [out("web", "https://a.example", "url")] }),
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
        outputs: [out("web", "https://git.box.yolab.io/", "url", "Open")],
      }),
      "box.yolab.io",
    );
    expect(links).toHaveLength(1);
  });

  it("ignores outputs that are not addresses", () => {
    const links = appLinks(
      app({ outputs: [out("password", "hunter2"), out("empty", "", "url")] }),
      "",
    );
    expect(links).toEqual([]);
  });

  it("offers nothing derived when there is no tunnel domain yet", () => {
    expect(appLinks(app({ config: { subdomain: "git" } }), "")).toEqual([]);
  });
});

describe("appFactRows", () => {
  it("shows a declared fact before its value exists", () => {
    const rows = appFactRows(
      app({ outputs_spec: [spec("temp_password", "Temporary password")] }),
    );
    expect(rows).toEqual([
      { key: "temp_password", label: "Temporary password", value: null },
    ]);
  });

  it("fills a declared fact in once it has been scraped", () => {
    const rows = appFactRows(
      app({
        outputs_spec: [spec("temp_password", "Temporary password")],
        outputs: [out("temp_password", "abc123")],
      }),
    );
    expect(rows[0].value).toBe("abc123");
  });

  it("keeps a scraped fact the chart no longer declares", () => {
    const rows = appFactRows(
      app({
        outputs_spec: [spec("user", "Username")],
        outputs: [out("legacy_password", "sekrit", "text", "Password")],
      }),
    );
    expect(rows.map((r) => r.key)).toEqual(["user", "legacy_password"]);
    expect(rows[1].value).toBe("sekrit");
  });

  it("leaves addresses and hidden values out — they are not facts to read", () => {
    const rows = appFactRows(
      app({
        outputs_spec: [
          spec("web", "Web", "url"),
          spec("secret", "S", "hidden"),
        ],
        outputs: [
          out("web", "https://a.example", "url"),
          out("secret", "x", "hidden"),
        ],
      }),
    );
    expect(rows).toEqual([]);
  });

  it("is ordered by the chart's spec, not by what happened to be scraped", () => {
    const rows = appFactRows(
      app({
        outputs_spec: [spec("a", "A"), spec("b", "B"), spec("c", "C")],
        outputs: [out("c", "3"), out("a", "1")],
      }),
    );
    expect(rows.map((r) => r.key)).toEqual(["a", "b", "c"]);
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
});

import { describe, expect, it } from "vitest";
import {
  adoptsDefault,
  connectionChoice,
  usedBy,
  usesOf,
} from "./connections";
import type { ServiceInstance } from "./services";

const ollama: ServiceInstance = {
  instance: "ai",
  namespace: "yolab-ai",
  app_id: "ollama",
  title: "Ollama API",
  url: "http://ollama.yolab-ai.svc.cluster.local:11434",
};

describe("choosing what an app connects to", () => {
  it("an app that can run its own does so until told otherwise", () => {
    expect(connectionChoice({}, [ollama], true, false)).toEqual({
      kind: "own",
    });
  });

  it("an app that needs one starts on the installed one and saves it", () => {
    const choice = connectionChoice({}, [ollama], false, false);
    expect(choice).toEqual({ kind: "installed", namespace: "yolab-ai" });
    expect(adoptsDefault({}, choice)).toBe("yolab-ai");
    expect(adoptsDefault({ from: "yolab-ai" }, choice)).toBeNull();
  });

  it("with nothing installed it asks for an address", () => {
    expect(connectionChoice({}, [], false, false)).toEqual({ kind: "typed" });
  });

  it("a saved connection to an app that is gone is shown as missing", () => {
    expect(
      connectionChoice({ from: "yolab-old" }, [ollama], true, false),
    ).toEqual({ kind: "missing", namespace: "yolab-old" });
  });

  it("an address typed by hand stays typed", () => {
    expect(
      connectionChoice(
        { from: "", api: "http://gaming-pc:11434" },
        [ollama],
        true,
        false,
      ),
    ).toEqual({ kind: "typed" });
  });
});

describe("which apps an app uses and is used by", () => {
  const base = {
    app_id: "x",
    chart_version: "1",
    status: "running" as const,
    detail: "",
    outputs: [],
    backup: { enabled: false, schedule: "", last_ok_at: null, running: false },
  };
  const ai = { ...base, instance_name: "ai", config: {} };
  const webui = {
    ...base,
    instance_name: "chat",
    config: { ollama: { from: "yolab-ai" }, subdomain: "chat" },
  };
  const typed = {
    ...base,
    instance_name: "notes",
    config: { ollama: { from: "", api: "http://pc:11434" } },
  };

  it("an app uses what its connections name", () => {
    expect(usesOf(webui)).toEqual(["ai"]);
    expect(usesOf(typed)).toEqual([]);
  });

  it("an app is used by every app connected to it", () => {
    expect(usedBy(ai, [ai, webui, typed])).toEqual(["chat"]);
    expect(usedBy(webui, [ai, webui, typed])).toEqual([]);
  });
});

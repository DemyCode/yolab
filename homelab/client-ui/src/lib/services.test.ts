import { describe, expect, it } from "vitest";
import { serviceChoice, serviceLabel, type ServiceInstance } from "./services";

const ollama: ServiceInstance = {
  instance: "ai",
  app_id: "ollama",
  title: "Ollama API",
  url: "http://ollama.yolab-ai.svc.cluster.local:11434",
};

describe("choosing a service for a link field", () => {
  it("an empty value means the app runs its own when it can", () => {
    expect(serviceChoice("", [ollama], true)).toEqual({ kind: "own" });
  });

  it("an app that cannot run its own starts on the first installed one", () => {
    expect(serviceChoice("", [ollama], false)).toEqual({
      kind: "installed",
      url: ollama.url,
    });
  });

  it("with nothing installed and no own copy, it asks for a URL", () => {
    expect(serviceChoice("", [], false)).toEqual({ kind: "url", url: "" });
  });

  it("a saved internal address is recognised as the installed app", () => {
    expect(serviceChoice(ollama.url, [ollama], true)).toEqual({
      kind: "installed",
      url: ollama.url,
    });
  });

  it("any other address is a URL typed by hand", () => {
    expect(serviceChoice("http://gaming-pc:11434", [ollama], true)).toEqual({
      kind: "url",
      url: "http://gaming-pc:11434",
    });
  });

  it("names an instance by itself, adding the app when they differ", () => {
    expect(serviceLabel(ollama)).toBe("ai (ollama)");
    expect(serviceLabel({ ...ollama, instance: "ollama" })).toBe("ollama");
  });
});

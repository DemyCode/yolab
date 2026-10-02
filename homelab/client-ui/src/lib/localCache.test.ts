import { beforeEach, describe, expect, it } from "vitest";
import { MAX_AGE_MS, forgetAll, recall, remember } from "./localCache";

describe("localCache", () => {
  beforeEach(() => {
    forgetAll();
    localStorage.clear();
  });

  it("gives back what it was told, with when it was true", () => {
    remember("apps", [{ id: "immich" }], 1_000);
    expect(recall("apps", 2_000)).toEqual({
      data: [{ id: "immich" }],
      savedAt: 1_000,
    });
  });

  it("survives a reload through localStorage", () => {
    remember("apps", ["a"], 1_000);
    const raw = localStorage.getItem("yolab:cache:v1:apps");
    forgetAll();
    localStorage.setItem("yolab:cache:v1:apps", raw ?? "");
    expect(recall("apps", 2_000)?.data).toEqual(["a"]);
  });

  it("never replaces a newer value with an older one", () => {
    remember("apps", ["new"], 5_000);
    remember("apps", ["old"], 1_000);
    expect(recall("apps", 6_000)?.data).toEqual(["new"]);
  });

  it("drops a value older than a week", () => {
    remember("apps", ["a"], 0);
    expect(recall("apps", MAX_AGE_MS + 1)).toBeNull();
  });

  it("forgets everything on sign-out, and only its own keys", () => {
    localStorage.setItem("yolab-theme", "dark");
    remember("apps", ["a"], 1_000);
    forgetAll();
    expect(recall("apps", 2_000)).toBeNull();
    expect(localStorage.getItem("yolab-theme")).toBe("dark");
  });

  it("treats a corrupt entry as nothing remembered", () => {
    localStorage.setItem("yolab:cache:v1:apps", "{not json");
    expect(recall("apps")).toBeNull();
  });
});

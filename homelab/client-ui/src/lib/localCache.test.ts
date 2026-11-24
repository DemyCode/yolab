import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
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

describe("localCache without working storage", () => {
  beforeEach(() => {
    forgetAll();
    localStorage.clear();
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("knows nothing about a key it was never told", () => {
    expect(recall("never-told")).toBeNull();
  });

  it("ignores a stored entry that is not a remembered value", () => {
    for (const raw of [
      "null",
      '{"data":1}',
      '{"savedAt":"x","data":1}',
      '{"savedAt":1}',
    ]) {
      localStorage.setItem("yolab:cache:v1:odd", raw);
      expect(recall("odd", 1), raw).toBeNull();
    }
  });

  it("still remembers in memory when there is no localStorage", () => {
    vi.stubGlobal("localStorage", undefined);
    remember("k", 1, 10);
    expect(recall("k", 10)).toEqual({ data: 1, savedAt: 10 });
    forgetAll();
    expect(recall("k", 10)).toBeNull();
  });

  it("still remembers in memory when localStorage refuses to be opened", () => {
    const real = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      get() {
        throw new Error("blocked");
      },
    });
    try {
      remember("k", 2, 10);
      expect(recall("k", 10)).toEqual({ data: 2, savedAt: 10 });
    } finally {
      Object.defineProperty(globalThis, "localStorage", real!);
    }
  });

  it("keeps the value in memory when localStorage is full", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    remember("k", 3, 10);
    expect(recall("k", 10)).toEqual({ data: 3, savedAt: 10 });
  });

  it("forgets from memory even when localStorage cannot be listed", () => {
    localStorage.setItem("unrelated", "x");
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    remember("k", 4, 10);
    vi.spyOn(Storage.prototype, "key").mockImplementation(() => {
      throw new Error("blocked");
    });
    forgetAll();
    expect(recall("k", 10)).toBeNull();
  });

  it("an old value is dropped even when localStorage cannot remove it", () => {
    remember("k", 5, 0);
    vi.spyOn(Storage.prototype, "removeItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(recall("k", MAX_AGE_MS + 1)).toBeNull();
  });

  it("a stored entry that cannot be read is nothing remembered", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(recall("k")).toBeNull();
  });
});

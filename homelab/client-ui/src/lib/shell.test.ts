import { describe, expect, it } from "vitest";
import { CHANGE_ADDRESS_URL, changeServer, inYolabShell } from "./shell";

describe("the YoLab app shell", () => {
  it("is detected only when the app marked the page", () => {
    expect(inYolabShell({})).toBe(false);
    expect(inYolabShell({ __YOLAB_SHELL__: true })).toBe(true);
  });

  it("change server opens the one link the app catches instead of loading", () => {
    const w = { location: { href: "https://cluster.1.yolab.io/system" } };
    changeServer(w);
    expect(w.location.href).toBe("yolab-shell://change-address");
    expect(CHANGE_ADDRESS_URL).toBe("yolab-shell://change-address");
  });
});

describe("the YoLab app shell in this window", () => {
  it("reads the real window by default", () => {
    expect(inYolabShell()).toBe(false);
  });

  it("changes this window's address by default", () => {
    const real = window.location;
    const fake = { href: "https://x" };
    Object.defineProperty(window, "location", {
      configurable: true,
      value: fake,
    });
    try {
      changeServer();
      expect(fake.href).toBe(CHANGE_ADDRESS_URL);
    } finally {
      Object.defineProperty(window, "location", {
        configurable: true,
        value: real,
      });
    }
  });
});

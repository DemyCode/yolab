import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { initTheme, readThemeChoice, useTheme } from "./theme";

function systemDark(dark: boolean) {
  const listeners = new Set<() => void>();
  const mq = {
    matches: dark,
    addEventListener: vi.fn((_: string, fn: () => void) => listeners.add(fn)),
    removeEventListener: vi.fn((_: string, fn: () => void) =>
      listeners.delete(fn),
    ),
  };
  vi.stubGlobal(
    "matchMedia",
    vi.fn(() => mq),
  );
  return {
    mq,
    flip(next: boolean) {
      mq.matches = next;
      listeners.forEach((fn) => fn());
    },
  };
}

const dark = () => document.documentElement.classList.contains("dark");

beforeEach(() => {
  localStorage.clear();
  document.documentElement.classList.remove("dark");
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("the theme", () => {
  it("follows the system until someone picks one", () => {
    expect(readThemeChoice()).toBe("system");
    localStorage.setItem("yolab.theme", "dark");
    expect(readThemeChoice()).toBe("dark");
    localStorage.setItem("yolab.theme", "light");
    expect(readThemeChoice()).toBe("light");
    localStorage.setItem("yolab.theme", "purple");
    expect(readThemeChoice()).toBe("system");
  });

  it("starts dark when the system is dark", () => {
    systemDark(true);
    initTheme();
    expect(dark()).toBe(true);
  });

  it("starts light when the browser cannot say what the system prefers", () => {
    vi.stubGlobal("matchMedia", undefined);
    document.documentElement.classList.add("dark");
    initTheme();
    expect(dark()).toBe(false);
  });

  it("a picked theme wins over the system and is remembered", () => {
    systemDark(true);
    localStorage.setItem("yolab.theme", "light");
    initTheme();
    expect(dark()).toBe(false);
  });

  it("switching is shown at once, remembered, and forgotten for system", () => {
    systemDark(false);
    const { result } = renderHook(() => useTheme());
    expect(result.current).toMatchObject({
      choice: "system",
      resolved: "light",
    });
    act(() => result.current.setTheme("dark"));
    expect(result.current).toMatchObject({ choice: "dark", resolved: "dark" });
    expect(localStorage.getItem("yolab.theme")).toBe("dark");
    expect(dark()).toBe(true);
    act(() => result.current.setTheme("system"));
    expect(localStorage.getItem("yolab.theme")).toBeNull();
    expect(dark()).toBe(false);
  });

  it("follows the system as it changes only while set to system", () => {
    const system = systemDark(false);
    const { result, unmount } = renderHook(() => useTheme());
    act(() => system.flip(true));
    expect(dark()).toBe(true);
    act(() => result.current.setTheme("light"));
    expect(system.mq.removeEventListener).toHaveBeenCalledOnce();
    act(() => system.flip(true));
    expect(dark()).toBe(false);
    unmount();
  });
});

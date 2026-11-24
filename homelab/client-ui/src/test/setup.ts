import { cleanup } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import { forgetPending } from "@/lib/inflight";
import { forgetAll } from "@/lib/localCache";

Object.defineProperty(window, "matchMedia", {
  configurable: true,
  writable: true,
  value: (query: string) => ({
    matches: false,
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
  }),
});

afterEach(() => {
  cleanup();
  forgetAll();
  forgetPending();
  localStorage.clear();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

import { cleanup } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import { forgetPending } from "@/lib/inflight";
import { forgetAll } from "@/lib/localCache";
import { forgetListings } from "@/lib/treeFolder";

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
  forgetListings();
  localStorage.clear();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

import { defineConfig } from "vitest/config";
import path from "path";
import untested from "./untested.json";

export default defineConfig({
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["src/test/setup.ts"],
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
    restoreMocks: true,
    coverage: {
      provider: "istanbul",
      include: ["src/**/*.{ts,tsx}"],
      exclude: [
        "src/**/*.test.{ts,tsx}",
        "src/**/*.d.ts",
        ...Object.keys(untested),
      ],
      reporter: [["text", { skipFull: true, maxCols: 300 }], "html"],
      thresholds: { 100: true },
    },
  },
});

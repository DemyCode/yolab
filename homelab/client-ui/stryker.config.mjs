import { readFileSync } from "node:fs";

const untested = Object.keys(
  JSON.parse(readFileSync(new URL("./untested.json", import.meta.url), "utf8")),
);

export default {
  testRunner: "vitest",
  vitest: { configFile: "vitest.config.ts" },
  mutate: [
    "src/**/*.{ts,tsx}",
    "!src/**/*.test.{ts,tsx}",
    "!src/**/*.d.ts",
    ...untested.map((f) => `!${f}`),
  ],
  coverageAnalysis: "perTest",
  reporters: ["clear-text", "html"],
  htmlReporter: { fileName: "reports/mutation/index.html" },
  thresholds: { high: 100, low: 100, break: 100 },
};

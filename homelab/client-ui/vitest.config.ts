import { defineConfig } from "vitest/config";
import path from "path";

// SEPARATE FROM vite.config.ts, DELIBERATELY.
//
// The obvious thing is to add a `test` block to vite.config.ts, and it does not
// work here: vitest 2 bundles vite 5's types while this project builds on vite
// 7, so `defineConfig` imported from "vitest/config" rejects the plugin array
// the production build needs. The failure is a bare "No overload matches this
// call" pointing at the plugins line, which says nothing about versions.
//
// Keeping the two apart means the production config stays exactly what it was
// and the test config carries only what tests need. The alias is repeated
// rather than imported for the same reason — importing it would drag the
// plugin types back in.
//
// No plugins here: everything under test so far is plain TypeScript. Component
// tests will need @vitejs/plugin-react, and that is the point at which the
// version mismatch has to be solved rather than stepped around.
export default defineConfig({
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
    restoreMocks: true,
  },
});

import { defineConfig } from "vitest/config";

// The state core (reducer, envelope parsing, visuals, perf governor) is plain
// TypeScript with no DOM/Tauri imports — node environment is sufficient.
export default defineConfig({
  test: {
    include: ["src/test/**/*.test.ts"],
    environment: "node",
  },
});

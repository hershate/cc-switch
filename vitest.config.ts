import path from "node:path";
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  test: {
    include: ["{src,tests}/**/*.{test,spec}.{ts,tsx,js,jsx}"],
    environment: "jsdom",
    setupFiles: ["./tests/setupGlobals.ts", "./tests/setupTests.ts"],
    globals: true,
    // vitest 2.1.x derives an implicit minWorkers (= CPU-1) when unset and
    // never clamps it below maxWorkers, so `vitest run --maxWorkers=16` on a
    // 32-core host crashes Tinypool with "minThreads and maxThreads must not
    // conflict". Pin the idle-pool floor; maxWorkers still caps parallelism.
    minWorkers: 1,
    coverage: {
      reporter: ["text", "lcov"],
    },
  },
});

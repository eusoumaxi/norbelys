import { readFileSync } from "node:fs";

import { cloudflare } from "@cloudflare/vite-plugin";
import babel from "@rolldown/plugin-babel";
import tailwindcss from "@tailwindcss/vite";
import { tanstackRouter } from "@tanstack/router-plugin/vite";
import react, { reactCompilerPreset } from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const contract: { paths: Record<string, unknown> } = JSON.parse(
  readFileSync(
    new URL("../../crates/server/openapi.json", import.meta.url),
    "utf-8"
  )
);

export default defineConfig({
  define: {
    __NORBELYS_API_ROUTES__: JSON.stringify(Object.keys(contract.paths)),
  },
  plugins: [
    // Must run before the React plugin: it generates src/routeTree.gen.ts.
    tanstackRouter({ autoCodeSplitting: true, target: "react" }),
    react(),
    babel({ presets: [reactCompilerPreset()] }),
    tailwindcss(),
    cloudflare(),
  ],
  // One .env.local for the whole repository (scripts/dev-init.sh). Only VITE_* reaches the bundle.
  envDir: "../..",
  // The favicons, app icons and web manifest are the brand's own files, served from the root.
  publicDir: "../../brand/icons",
  // `@/*`, `@brand/*` and the SDK source come from tsconfig.app.json.
  resolve: { tsconfigPaths: true },
  server: {
    port: 5173,
    strictPort: true,
  },
});

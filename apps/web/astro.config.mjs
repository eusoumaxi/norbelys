import { satteri } from "@astrojs/markdown-satteri";
import { defineConfig } from "astro/config";

import { clauses } from "./src/clauses.ts";

export default defineConfig({
  devToolbar: { enabled: false },
  // The team pages live under /for/; the folder itself is the solutions page.
  redirects: {
    "/for": "/solutions",
  },
  markdown: {
    // Legal documents are laid out as clauses, each with its plain-words note (src/clauses.ts).
    processor: satteri({ hastPlugins: [clauses] }),
    // Code in posts takes its colours from the site's own tokens (styles/blog.css).
    shikiConfig: { theme: "css-variables" },
  },
  server: {
    port: 4321,
    strictPort: true,
  },
  site: "https://norbelys.com",
});

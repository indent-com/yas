import { fileURLToPath } from "node:url";

import nimbus, {
  defineConfig as defineNimbusConfig,
} from "@cloudflare/nimbus-docs";
import { tableScroll } from "@cloudflare/nimbus-docs/markdown";
import tailwindcss from "@tailwindcss/vite";
import icon from "astro-icon";
import { defineConfig, fontProviders } from "astro/config";

const nimbusConfig = defineNimbusConfig({
  site: "https://docs.yas.run",
  title: "yas",
  description:
    "Documentation for yas, a terminal multiplexer and experimental Wayland compositor for browsers and AI agents.",
  locale: "en",
  socialImageAlt: "yas documentation preview",
});

export default defineConfig({
  output: "static",
  // Inter, self-hosted. One variable file covers 100 to 900 plus the opsz
  // axis, so `font-optical-sizing` gives display-cut headings without a second
  // family.
  fonts: [
    {
      name: "Inter",
      cssVariable: "--font-inter",
      provider: fontProviders.local(),
      options: {
        variants: [
          {
            src: [
              "./node_modules/@fontsource-variable/inter/files/inter-latin-standard-normal.woff2",
            ],
            weight: "100 900",
            style: "normal",
          },
        ],
      },
    },
  ],
  // Tailwind v4 through its Vite plugin; the PostCSS plugin does not build
  // under Astro 7's Vite 8 bundler.
  vite: {
    plugins: [tailwindcss()],
    // Pages import repository files (the `yas learn` guide, the generated
    // wire registry) from outside this project.
    server: {
      fs: { allow: [fileURLToPath(new URL("../..", import.meta.url))] },
    },
  },
  // Hover-prefetch link targets so full-page navigations feel instant without
  // a client-side router.
  prefetch: {
    prefetchAll: true,
    defaultStrategy: "hover",
  },
  integrations: [
    icon(),
    nimbus(nimbusConfig, {
      // Invalid frontmatter breaks rendering and broken internal links are
      // 404s, so both fail the build. See `nimbus-docs lint --help` for more.
      rules: {
        "nimbus/frontmatter-shape": "error",
        "nimbus/internal-link": "error",
      },
      // Wrap wide tables so they scroll instead of overflowing the page
      // (styled by `.nb-table-scroll` in src/styles/prose.css).
      markdown: {
        hastPlugins: [tableScroll()],
      },
    }),
  ],
});

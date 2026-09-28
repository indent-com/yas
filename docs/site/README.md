# yas docs (docs.yas.run)

The public documentation: an [Astro](https://astro.build) site built on
[`@cloudflare/nimbus-docs`](https://nimbus-docs.com), one `.mdx` per page under
`src/content/docs/`. It builds to static HTML and is hosted on Vercel, with this directory as the
project root.

## Install and build

This project has its own lockfile and is not part of the `js/` pnpm workspace. The Nix dev shell
supplies Node and pnpm.

```sh
pnpm --dir docs/site install --ignore-workspace --frozen-lockfile --ignore-scripts
bin/dev-docs 4321                    # dev server with live reload
pnpm --dir docs/site run build       # static build to dist/
pnpm --dir docs/site run check       # typecheck, build, and nimbus-docs lint (the CI gate)
bin/generate-cli-docs                # regenerate reference/cli from crates/cli/src/cli.rs
bin/check-env-docs                   # fail if a YAS_* variable read by code is undocumented
```

- Invalid frontmatter and broken internal links fail the build.
- Pages import `crates/cli/src/learn.md` and `protocol/yas/wire.md` at build time, so the site
  builds only from inside a yas checkout.
- Writing rules, terminology, and page structure are in [AGENTS.md](AGENTS.md).

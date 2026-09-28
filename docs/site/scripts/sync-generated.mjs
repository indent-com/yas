// Copies repository Markdown that has a canonical home elsewhere into the
// content collection before every build, so the site never drifts from it:
// the `yas learn` guide and the generated wire registry. The outputs are
// gitignored.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const site = join(dirname(fileURLToPath(import.meta.url)), "..");
const repo = join(site, "../..");
const content = join(site, "src/content/docs");

const pages = [
  {
    source: "crates/cli/src/learn.md",
    target: "agents/learn.mdx",
    title: "yas learn",
    description:
      "The CLI guide for scripts and agents that `yas learn` prints, rendered from the same file.",
    order: 72,
    intro:
      "This is the text `yas learn` prints, rendered from [`crates/cli/src/learn.md`](https://github.com/yas-run/yas/blob/main/crates/cli/src/learn.md). Point an agent at `yas learn` on the machine it works on, so it reads the guide for the installed release.",
  },
  {
    source: "protocol/yas/wire.md",
    target: "reference/wire-registry.mdx",
    title: "Wire registry",
    description:
      "Every family, message, and record layout in the yas protocol, generated from the canonical schema.",
    order: 126,
    intro:
      "This page renders [`protocol/yas/wire.md`](https://github.com/yas-run/yas/blob/main/protocol/yas/wire.md), which `crates/yas/schema_codegen.rs` generates from the TOML schema in `protocol/yas/`. Layout text is normative. Resource lifecycle rules are in the [protocol specification](https://github.com/yas-run/yas/blob/main/docs/design/yas.md); [The yas protocol](/internals/protocol) is the overview.",
  },
];

// MDX parses `{`, `}`, and `<` as JSX outside code; HTML comments are invalid.
function toMdx(markdown) {
  const out = [];
  let fence = null;
  for (const line of markdown.replace(/<!--[\s\S]*?-->\n?/g, "").split("\n")) {
    const opener = line.match(/^\s*(`{3,}|~{3,})/);
    if (fence) {
      if (opener && line.trim().startsWith(fence)) fence = null;
      out.push(line);
      continue;
    }
    if (opener) {
      fence = opener[1];
      out.push(line);
      continue;
    }
    out.push(
      line
        .split(/(`+[^`]*`+)/)
        .map((part, i) =>
          i % 2
            ? part
            : part.replace(/[{}]/g, (c) => `\\${c}`).replace(/</g, "&lt;"),
        )
        .join(""),
    );
  }
  return out.join("\n");
}

for (const page of pages) {
  const body = readFileSync(join(repo, page.source), "utf8").replace(
    /^(<!--[\s\S]*?-->\n)*# .*\n+/,
    "",
  );
  const frontmatter = [
    "---",
    `title: ${JSON.stringify(page.title)}`,
    `description: ${JSON.stringify(page.description)}`,
    "sidebar:",
    `  order: ${page.order}`,
    "---",
    "",
    page.intro,
    "",
  ].join("\n");
  const target = join(content, page.target);
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, `${frontmatter}\n${toMdx(body)}`);
}

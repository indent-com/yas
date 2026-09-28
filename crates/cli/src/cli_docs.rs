//! Renders the clap command tree as MDX pages for the docs site
//! (`docs/site/src/content/docs/reference/cli/`). `yas generate --markdown DIR`
//! writes them, and a unit test fails when the checked-in pages are stale.

use crate::cli;
use clap::{Arg, ArgAction, Command, CommandFactory};

const HEADER: &str =
    "{/* Generated from crates/cli/src/cli.rs by `bin/generate-cli-docs`. Do not edit. */}";

/// Every page as `(file name, contents)`, index first.
pub fn render() -> Vec<(String, String)> {
    let mut root = cli::Cli::command();
    root.build();
    let commands: Vec<&Command> = visible_subcommands(&root).collect();

    let mut pages = vec![("index.mdx".to_string(), render_index(&root, &commands))];
    for (i, cmd) in commands.iter().enumerate() {
        pages.push((
            format!("{}.mdx", cmd.get_name()),
            render_command_page(cmd, i + 1),
        ));
    }
    pages
}

fn render_index(root: &Command, commands: &[&Command]) -> String {
    let mut out = String::new();
    out.push_str(&frontmatter(
        "CLI reference",
        "Every yas subcommand, argument, and option, generated from the CLI's own definitions.",
        "  order: 121\n  group:\n    label: \"CLI reference\"\n",
    ));
    out.push_str(HEADER);
    out.push_str("\n\n");
    out.push_str(
        "This reference is generated from the same definitions that produce `yas --help`, so it \
         matches the release built from this commit. Run `yas <command> --help` for the same text \
         in a terminal, or `yas learn` for a task-oriented guide.\n\n",
    );
    out.push_str("```text\n");
    out.push_str(&usage(root));
    out.push_str("\n```\n\n## Global options\n\nThese options apply to every subcommand.\n\n");
    for arg in visible_args(root) {
        push_arg(&mut out, arg);
    }
    out.push_str("\n## Commands\n\n");
    for cmd in commands {
        out.push_str(&format!(
            "- [`yas {}`](/reference/cli/{}): {}\n",
            cmd.get_name(),
            cmd.get_name(),
            escape(&mark_code(&sentence(&about(cmd))))
        ));
    }
    out
}

fn render_command_page(cmd: &Command, order: usize) -> String {
    let mut out = String::new();
    let about = about(cmd);
    out.push_str(&frontmatter(
        &format!("yas {}", cmd.get_name()),
        &sentence(&about),
        &format!("  order: {order}\n"),
    ));
    out.push_str(HEADER);
    out.push_str("\n\n");
    push_body(&mut out, cmd, 2);
    out
}

fn push_body(out: &mut String, cmd: &Command, depth: usize) {
    if let Some(long) = cmd.get_long_about() {
        push_text(out, &long.to_string(), "");
    } else if let Some(short) = cmd.get_about() {
        push_text(out, &sentence(&short.to_string()), "");
    }

    let aliases: Vec<&str> = cmd.get_visible_aliases().collect();
    if !aliases.is_empty() {
        let list: Vec<String> = aliases.iter().map(|a| format!("`{a}`")).collect();
        out.push_str(&format!("Alias: {}.\n\n", list.join(", ")));
    }

    out.push_str("```text\n");
    out.push_str(&usage(cmd));
    out.push_str("\n```\n\n");

    let (positional, options): (Vec<&Arg>, Vec<&Arg>) = visible_args(cmd)
        .filter(|a| !a.is_global_set())
        .partition(|a| a.is_positional());
    if !positional.is_empty() {
        out.push_str("Arguments:\n\n");
        for arg in positional {
            push_arg(out, arg);
        }
    }
    if !options.is_empty() {
        out.push_str("Options:\n\n");
        for arg in options {
            push_arg(out, arg);
        }
    }
    if let Some(after) = cmd.get_after_long_help().or(cmd.get_after_help()) {
        push_text(out, &after.to_string(), "");
    }

    for sub in visible_subcommands(cmd) {
        let hashes = "#".repeat(depth.min(6));
        let name = sub.get_bin_name().unwrap_or(sub.get_name());
        out.push_str(&format!("{hashes} {name}\n\n"));
        push_body(out, sub, depth + 1);
    }
}

fn visible_subcommands(cmd: &Command) -> impl Iterator<Item = &Command> {
    cmd.get_subcommands()
        .filter(|c| !c.is_hide_set() && c.get_name() != "help")
}

fn visible_args(cmd: &Command) -> impl Iterator<Item = &Arg> {
    cmd.get_arguments()
        .filter(|a| !a.is_hide_set())
        .filter(|a| {
            !matches!(
                a.get_action(),
                ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
            )
        })
}

fn push_arg(out: &mut String, arg: &Arg) {
    out.push_str(&format!("- `{}`\n\n", arg_label(arg)));
    let help = arg
        .get_long_help()
        .or(arg.get_help())
        .map(|h| h.to_string())
        .unwrap_or_default();
    if !help.trim().is_empty() {
        push_text(out, &help, "  ");
    }

    let mut facts = Vec::new();
    if arg.is_required_set() && !arg.is_positional() {
        facts.push("Required.".to_string());
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|v| !v.is_empty())
        .collect();
    if !defaults.is_empty() && !matches!(arg.get_action(), ArgAction::SetTrue | ArgAction::SetFalse)
    {
        let list: Vec<String> = defaults.iter().map(|d| format!("`{d}`")).collect();
        facts.push(format!("Default: {}.", list.join(", ")));
    }
    let possible: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|p| !p.is_hide_set())
        .map(|p| format!("`{}`", p.get_name()))
        .collect();
    if !possible.is_empty() && !matches!(arg.get_action(), ArgAction::SetTrue | ArgAction::SetFalse)
    {
        facts.push(format!("Values: {}.", possible.join(", ")));
    }
    if let Some(env) = arg.get_env() {
        facts.push(format!("Environment: `{}`.", env.to_string_lossy()));
    }
    if !facts.is_empty() {
        out.push_str(&format!("  {}\n\n", facts.join(" ")));
    }
}

fn arg_label(arg: &Arg) -> String {
    let names: Vec<String> = arg
        .get_value_names()
        .map(|v| v.iter().map(|n| n.to_string()).collect())
        .unwrap_or_else(|| vec![arg.get_id().as_str().to_uppercase()]);
    let multiple = arg.get_num_args().is_some_and(|n| n.max_values() > 1)
        || matches!(arg.get_action(), ArgAction::Append);
    if arg.is_positional() {
        let inner = names.join(" ");
        let base = if arg.is_required_set() {
            format!("<{inner}>")
        } else {
            format!("[{inner}]")
        };
        return if multiple { format!("{base}...") } else { base };
    }
    let mut label = match (arg.get_short(), arg.get_long()) {
        (Some(s), Some(l)) => format!("-{s}, --{l}"),
        (Some(s), None) => format!("-{s}"),
        (None, Some(l)) => format!("--{l}"),
        (None, None) => arg.get_id().to_string(),
    };
    if arg.get_action().takes_values() {
        for n in &names {
            label.push_str(&format!(" <{n}>"));
        }
    }
    label
}

fn usage(cmd: &Command) -> String {
    cmd.clone().render_usage().to_string().trim().to_string()
}

fn about(cmd: &Command) -> String {
    cmd.get_about().map(|a| a.to_string()).unwrap_or_default()
}

fn sentence(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() || t.ends_with(['.', '!', '?', ')']) {
        t.to_string()
    } else {
        format!("{t}.")
    }
}

fn frontmatter(title: &str, description: &str, sidebar: &str) -> String {
    format!(
        "---\ntitle: {}\ndescription: {}\nsidebar:\n{sidebar}---\n\n",
        yaml_string(title),
        yaml_string(description)
    )
}

fn yaml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Emits help text paragraph by paragraph. Paragraphs whose lines are
/// indented (examples, aligned tables) become `text` blocks; the rest are
/// reflowed prose with MDX-significant characters escaped.
fn push_text(out: &mut String, text: &str, indent: &str) {
    for para in text.split("\n\n") {
        let lines: Vec<&str> = para.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            continue;
        }
        let preformatted = lines
            .iter()
            .skip(1)
            .any(|l| l.starts_with(' ') || l.starts_with('\t'))
            || lines.iter().any(|l| l.starts_with("  "));
        if preformatted {
            let (lead, block) = if lines[0].starts_with(' ') || lines[0].starts_with('\t') {
                (None, &lines[..])
            } else {
                (Some(lines[0]), &lines[1..])
            };
            if let Some(lead) = lead {
                out.push_str(&format!("{indent}{}\n\n", escape(&mark_code(lead.trim()))));
            }
            if block.is_empty() {
                continue;
            }
            let min = block
                .iter()
                .map(|l| l.len() - l.trim_start().len())
                .min()
                .unwrap_or(0);
            out.push_str(&format!("{indent}```text\n"));
            for l in block {
                out.push_str(&format!("{indent}{}\n", l[min..].trim_end()));
            }
            out.push_str(&format!("{indent}```\n\n"));
        } else {
            let joined = lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
            if let Some((lead, examples)) = split_examples(&joined) {
                out.push_str(&format!("{indent}{lead}\n\n{indent}```text\n"));
                for example in examples {
                    out.push_str(&format!("{indent}{example}\n"));
                }
                out.push_str(&format!("{indent}```\n\n"));
            } else {
                out.push_str(&format!("{indent}{}\n\n", escape(&mark_code(&joined))));
            }
        }
    }
}

/// clap reflows doc comments, so an "Examples: yas a yas b" paragraph arrives
/// as one line. Splits it back into one command per line.
fn split_examples(text: &str) -> Option<(String, Vec<String>)> {
    let (at, label) = ["Examples: yas ", "Example: yas "]
        .iter()
        .find_map(|label| text.find(label).map(|at| (at, *label)))?;
    let label_len = label.len() - "yas ".len();
    let lead = format!(
        "{}{}",
        escape(&mark_code(&text[..at])),
        &text[at..at + label_len]
    );
    let examples = text[at + label_len..]
        .trim()
        .split("yas ")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| format!("yas {part}"))
        .collect();
    Some((lead.trim().to_string(), examples))
}

/// Wraps flags, `$VARIABLES`, and `YAS_*` names in code spans so Markdown
/// typography leaves them alone (`--shell` would otherwise become an en dash).
fn mark_code(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_code = false;
    for word in text.split(' ') {
        let ticks = word.matches('`').count();
        if in_code || ticks > 0 {
            if ticks % 2 == 1 {
                in_code = !in_code;
            }
            out.push(word.to_string());
            continue;
        }
        let start = word
            .find(|c: char| c != '(' && c != '"' && c != '\'')
            .unwrap_or(word.len());
        let (open, body) = word.split_at(start);
        let end = body
            .trim_end_matches(['.', ',', ';', ':', ')', '"', '\''])
            .len();
        let (core, close) = body.split_at(end);
        let is_flag = core == "--"
            || (core.starts_with("--")
                && core.len() > 2
                && core.as_bytes()[2].is_ascii_alphabetic())
            || (core.len() == 2
                && core.starts_with('-')
                && core.as_bytes()[1].is_ascii_alphabetic());
        let is_var =
            core.len() > 1 && core.starts_with('$') && core.as_bytes()[1].is_ascii_uppercase();
        let is_env = core.starts_with("YAS_") && core.len() > 4;
        if is_flag || is_var || is_env {
            out.push(format!("{open}`{core}`{close}"));
        } else {
            out.push(word.to_string());
        }
    }
    out.join(" ")
}

/// Escapes characters MDX or Markdown would interpret, outside code spans.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code = false;
    for ch in text.chars() {
        if ch == '`' {
            in_code = !in_code;
            out.push(ch);
            continue;
        }
        if in_code {
            out.push(ch);
            continue;
        }
        match ch {
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '*' => out.push_str("\\*"),
            _ => out.push(ch),
        }
    }
    if in_code {
        // An unbalanced backtick would swallow the rest of the page.
        return escape(&text.replace('`', "'"));
    }
    out
}

pub fn write(dir: &str) {
    let dir = std::path::Path::new(dir);
    std::fs::create_dir_all(dir)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", dir.display()));
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "mdx") {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    for (name, contents) in render() {
        let path = dir.join(name);
        std::fs::write(&path, contents)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    const SITE_DIR: &str = "docs/site/src/content/docs/reference/cli";

    #[test]
    fn checked_in_cli_reference_is_current() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(SITE_DIR);
        if std::env::var_os("YAS_UPDATE_CLI_DOCS").as_deref() == Some(std::ffi::OsStr::new("1")) {
            write(dir.to_str().unwrap());
            return;
        }
        let expected: BTreeMap<String, String> = render().into_iter().collect();
        let mut actual = BTreeMap::new();
        for entry in std::fs::read_dir(&dir)
            .expect("reference/cli is missing; run bin/generate-cli-docs")
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "mdx") {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                actual.insert(name, std::fs::read_to_string(&path).unwrap());
            }
        }
        let stale: Vec<&String> = expected
            .keys()
            .chain(actual.keys())
            .filter(|k| expected.get(*k) != actual.get(*k))
            .collect();
        assert!(
            stale.is_empty(),
            "stale CLI reference pages {stale:?}; run bin/generate-cli-docs"
        );
    }

    #[test]
    fn mark_code_wraps_flags_and_variables() {
        assert_eq!(
            mark_code("Pass --shell to run through $SHELL (see YAS_SOCK)."),
            "Pass `--shell` to run through `$SHELL` (see `YAS_SOCK`)."
        );
        assert_eq!(mark_code("a `--x y` -- b"), "a `--x y` `--` b");
    }

    #[test]
    fn escape_leaves_code_spans_alone() {
        assert_eq!(escape("a {b} `c {d}` <e>"), "a \\{b\\} `c {d}` &lt;e&gt;");
    }
}

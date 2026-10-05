//! `cargo xtask purity`: `crates/server/src/domain/` stays pure, and maintenance tooling outside the Python SDK remains Rust or TypeScript.
//!
//! The domain module holds the product's decisions (pacing, retry waits, verdicts, the rules
//! a value must satisfy) as plain functions over plain values: the caller passes in the facts
//! and the current instant, and gets a decision back. That is what lets those decisions be
//! tested exhaustively with values generated from the enums, and reused by every role, without
//! a database, a network or a clock. One import of an I/O crate would quietly end that, so this
//! check refuses, in every file of `domain/` (and in `domain.rs`), any path that names:
//!
//! | Path | What it reaches |
//! |---|---|
//! | `sqlx::…` | the database |
//! | `axum::…` | HTTP serving |
//! | `tokio::…` | the async runtime |
//! | `reqwest::…` | HTTP requests |
//! | `lettre::…` | mail transport |
//! | `std::env…` | the process environment |
//! | `std::time::SystemTime`, `std::time::UNIX_EPOCH`, `jiff::Timestamp::now`, `jiff::Zoned::now` | the wall clock |
//! | `std::time::Instant` | the monotonic clock |
//!
//! Where a path can hide, and how each is found:
//!
//! - `use` declarations, with groups, renames and `self` expanded (`use std::{env, fmt}`);
//! - `extern crate`;
//! - fully qualified paths anywhere in types, expressions and patterns (`::sqlx::PgPool`);
//! - paths through a `use` alias, expanded before the comparison (`use std::time;` followed by
//!   `time::SystemTime::now()` is `std::time::SystemTime::now`);
//! - the token streams of macro calls and attribute arguments, which the parser keeps as tokens
//!   rather than paths (`format!("{:?}", std::env::var("X"))`, `#[derive(sqlx::Type)]`);
//! - glob imports that could bring one of these into scope (`use std::time::*`), refused as such.
//!
//! The check reads source, not the compiler's name resolution: it knows nothing of a crate
//! renamed in `Cargo.toml`, and it treats every alias of a file as visible in the whole file,
//! which can only add findings. Tests are included: a policy test has no more need of I/O than
//! the policy it tests. Storage encodings of domain types (`impl sqlx::Type for Id<R>`) belong in
//! `crates/server/src/db/types.rs`, which is what keeps this rule and Rust's orphan rule
//! compatible.
//!
//! **One SDK boundary.** Python source and tests belong under `sdks/python/`. Outside that
//! package the command fails on a tracked file that is Python: a name ending
//! in `.py`, or a first line that is a `#!` naming `python` (`#!/usr/bin/env python3` on a
//! script without an extension). The repository's own tooling is Rust (`tools/xtask`) and, for
//! the JavaScript side, Bun TypeScript; a Python script left behind would be a second, untested
//! way of doing what one of those does, and a runtime every machine and CI image would have to
//! carry. The Python SDK is an intentional product package with its own locked dependencies
//! and checks. Only tracked files count (`git ls-files`):
//! an untracked or ignored file is no one else's concern until it is added.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use proc_macro2::{Spacing, TokenStream, TokenTree};
use syn::visit::{self, Visit};

/// The path prefixes `domain/` may not name, each with what it reaches.
const FORBIDDEN: &[(&[&str], &str)] = &[
    (&["crate", "db"], "application persistence"),
    (&["crate", "identity"], "application identity operations"),
    (&["crate", "roles"], "process startup"),
    (&["crate", "process"], "process lifecycle and wall clock"),
    (&["sqlx"], "the database"),
    (&["axum"], "HTTP serving"),
    (&["tokio"], "the async runtime"),
    (&["reqwest"], "HTTP requests"),
    (&["lettre"], "mail transport"),
    (&["std", "env"], "the process environment"),
    (&["std", "time", "SystemTime"], "the wall clock"),
    (&["std", "time", "UNIX_EPOCH"], "the wall clock"),
    (&["std", "time", "Instant"], "the monotonic clock"),
    (&["jiff", "Timestamp", "now"], "the wall clock"),
    (&["jiff", "Zoned", "now"], "the wall clock"),
];

/// How many times an alias is expanded: enough for an alias of an alias, and a stop for a
/// `use` that names itself.
const ALIAS_DEPTH: usize = 4;

/// How many bytes of a tracked file are read to find its `#!` line: an interpreter line is far
/// shorter, and reading no more keeps the scan of the whole tree cheap.
const SHEBANG_BYTES: u64 = 256;

/// One refused path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Violation {
    /// The 1-based line of the path's first segment.
    pub line: usize,
    /// The 1-based column of the path's first segment.
    pub column: usize,
    /// The path as it resolves, segments joined by `::`, with `::*` for a glob.
    pub path: String,
    /// What the path reaches.
    pub reaches: &'static str,
}

/// Checks every file of `crates/server/src/domain/` and `domain.rs`, one finding per refused
/// path, then every tracked file, one finding per Python file.
///
/// # Errors
///
/// The directory cannot be read, a file is not valid Rust (the compiler would refuse it too), or
/// git cannot list the tracked files.
pub fn run(root: &Path) -> anyhow::Result<Vec<String>> {
    let src = root.join("crates/server/src");
    let mut files = vec![src.join("domain.rs")];
    collect(&src.join("domain"), &mut files)
        .with_context(|| format!("cannot read {}", src.join("domain").display()))?;
    files.sort();
    let mut findings = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        let violations =
            check_source(&text).with_context(|| format!("{} is not valid Rust", file.display()))?;
        let name = crate::relative(root, file);
        findings.extend(violations.into_iter().map(|v| {
            format!(
                "{name}:{}:{}: `{}` reaches {}; domain/ holds pure policy, which takes such \
                 inputs as arguments",
                v.line, v.column, v.path, v.reaches
            )
        }));
    }
    let tracked = crate::git(root, &["ls-files", "-z"])?;
    for path in tracked.split('\0').filter(|path| !path.is_empty()) {
        findings.extend(python(path, &head(&root.join(path))));
    }
    Ok(findings)
}

/// The first [`SHEBANG_BYTES`] of the file at `path`, or nothing when it cannot be read: a
/// tracked file deleted from the working tree, or a submodule's directory, has no `#!` line.
fn head(path: &Path) -> Vec<u8> {
    let mut head = Vec::new();
    if let Ok(file) = std::fs::File::open(path)
        && file.take(SHEBANG_BYTES).read_to_end(&mut head).is_err()
    {
        head.clear();
    }
    head
}

/// The finding about the tracked file `path`, whose first bytes are `head`, when it is Python:
/// its name ends in `.py`, or its first line is a `#!` naming `python`.
pub fn python(path: &str, head: &[u8]) -> Option<String> {
    if path.starts_with("sdks/python/") {
        return None;
    }
    let first = head.split(|&byte| byte == b'\n').next().unwrap_or_default();
    if path.ends_with(".py") {
        Some(format!(
            "{path}: a Python file; the repository's tooling is Rust (tools/xtask) and Bun \
             TypeScript, never Python"
        ))
    } else if first.starts_with(b"#!") && first.windows(6).any(|word| word == b"python") {
        Some(format!(
            "{path}: a Python script (its `#!` line runs python); the repository's tooling is \
             Rust (tools/xtask) and Bun TypeScript, never Python"
        ))
    } else {
        None
    }
}

/// Every `.rs` file under `dir`, recursively.
fn collect(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// The refused paths of one source file, in order of appearance, each once.
///
/// # Errors
///
/// The source is not valid Rust.
pub fn check_source(source: &str) -> syn::Result<Vec<Violation>> {
    let file = syn::parse_file(source)?;
    let mut collector = Collector::default();
    collector.visit_file(&file);
    let aliases: BTreeMap<String, Vec<String>> = collector
        .uses
        .iter()
        .filter_map(|leaf| match leaf {
            Named::Import { path, alias, .. } => alias.clone().map(|alias| (alias, path.clone())),
            Named::Glob { .. } | Named::Path { .. } => None,
        })
        .collect();
    let mut seen = BTreeSet::new();
    let mut violations = Vec::new();
    for named in collector.uses.iter().chain(&collector.paths) {
        let (segments, glob, at) = match named {
            Named::Import { path, at, .. } => (expand(path, &aliases), false, *at),
            Named::Glob { prefix, at } => (expand(prefix, &aliases), true, *at),
            Named::Path { segments, at } => (expand(segments, &aliases), false, *at),
        };
        let Some(reaches) = refused(&segments, glob) else {
            continue;
        };
        let mut path = segments.join("::");
        if glob {
            path.push_str("::*");
        }
        if seen.insert((at, path.clone())) {
            violations.push(Violation {
                line: at.0,
                column: at.1,
                path,
                reaches,
            });
        }
    }
    violations.sort();
    Ok(violations)
}

/// `segments` with its first segment replaced by what a `use` alias of that name imports.
fn expand(segments: &[String], aliases: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut current = segments.to_vec();
    for _ in 0..ALIAS_DEPTH {
        let Some((head, rest)) = current.split_first() else {
            break;
        };
        let Some(target) = aliases.get(head) else {
            break;
        };
        if target.len() == 1 && target.first() == Some(head) {
            break;
        }
        let mut next = target.clone();
        next.extend_from_slice(rest);
        current = next;
    }
    current
}

/// What a path reaches when it is refused: it starts with a forbidden prefix, or, for a glob,
/// a forbidden path starts with it (the glob would import that item).
fn refused(segments: &[String], glob: bool) -> Option<&'static str> {
    FORBIDDEN.iter().find_map(|(prefix, reaches)| {
        let within = starts_with(segments, prefix);
        let imports = glob && prefix.len() > segments.len() && starts_with_owned(prefix, segments);
        (within || imports).then_some(*reaches)
    })
}

/// True when `segments` begins with `prefix`.
fn starts_with(segments: &[String], prefix: &[&str]) -> bool {
    segments.len() >= prefix.len() && segments.iter().zip(prefix).all(|(a, b)| a == b)
}

/// True when `prefix` (a forbidden path) begins with `segments`.
fn starts_with_owned(prefix: &[&str], segments: &[String]) -> bool {
    prefix.len() >= segments.len() && prefix.iter().zip(segments).all(|(a, b)| a == b)
}

/// A path named in the source, with its line and column.
#[derive(Debug)]
enum Named {
    /// A name a `use` declaration imports, and the alias it is visible under (`None` for `_`).
    Import {
        path: Vec<String>,
        alias: Option<String>,
        at: (usize, usize),
    },
    /// A glob import.
    Glob {
        prefix: Vec<String>,
        at: (usize, usize),
    },
    /// Any other path: in a type, an expression, a pattern, a macro or an attribute.
    Path {
        segments: Vec<String>,
        at: (usize, usize),
    },
}

/// Walks a file and records every path it names.
#[derive(Default)]
struct Collector {
    uses: Vec<Named>,
    paths: Vec<Named>,
}

/// The 1-based line and column of a span's start.
fn position(span: proc_macro2::Span) -> (usize, usize) {
    let start = span.start();
    (start.line, start.column.saturating_add(1))
}

impl Collector {
    /// Records the names a `use` tree imports, under `prefix`.
    fn use_tree(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.use_tree(&path.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Name(name) => {
                let at = position(name.ident.span());
                let (path, alias) = if name.ident == "self" {
                    (prefix.clone(), prefix.last().cloned())
                } else {
                    let mut path = prefix.clone();
                    path.push(name.ident.to_string());
                    (path, Some(name.ident.to_string()))
                };
                self.uses.push(Named::Import { path, alias, at });
            }
            syn::UseTree::Rename(rename) => {
                let at = position(rename.ident.span());
                let mut path = prefix.clone();
                if rename.ident != "self" {
                    path.push(rename.ident.to_string());
                }
                let alias = (rename.rename != "_").then(|| rename.rename.to_string());
                self.uses.push(Named::Import { path, alias, at });
            }
            syn::UseTree::Glob(glob) => {
                let [star] = glob.star_token.spans;
                let at = position(star);
                self.uses.push(Named::Glob {
                    prefix: prefix.clone(),
                    at,
                });
            }
            syn::UseTree::Group(group) => {
                for tree in &group.items {
                    self.use_tree(tree, prefix);
                }
            }
        }
    }

    /// Records the paths inside a token stream that the parser did not parse: a macro's
    /// arguments, an attribute's list. A path is a run of identifiers joined by `::`.
    fn tokens(&mut self, tokens: TokenStream) {
        let mut segments: Vec<String> = Vec::new();
        let mut at = (0, 0);
        let mut colons = 0_u8;
        for tree in tokens {
            match tree {
                TokenTree::Ident(ident) => {
                    // An identifier continues the path only right after `::`.
                    if colons != 2 {
                        self.flush(&mut segments, at);
                    }
                    if segments.is_empty() {
                        at = position(ident.span());
                    }
                    segments.push(ident.to_string());
                    colons = 0;
                }
                TokenTree::Punct(punct) if punct.as_char() == ':' => {
                    colons = if colons == 1 && punct.spacing() == Spacing::Alone {
                        2
                    } else if punct.spacing() == Spacing::Joint {
                        1
                    } else {
                        self.flush(&mut segments, at);
                        0
                    };
                }
                TokenTree::Group(group) => {
                    self.flush(&mut segments, at);
                    colons = 0;
                    self.tokens(group.stream());
                }
                TokenTree::Punct(_) | TokenTree::Literal(_) => {
                    self.flush(&mut segments, at);
                    colons = 0;
                }
            }
        }
        self.flush(&mut segments, at);
    }

    /// Records the path being collected, if any, and starts a new one.
    fn flush(&mut self, segments: &mut Vec<String>, at: (usize, usize)) {
        if !segments.is_empty() {
            self.paths.push(Named::Path {
                segments: std::mem::take(segments),
                at,
            });
        }
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.use_tree(&item.tree, &mut Vec::new());
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        let at = position(item.ident.span());
        let alias = item
            .rename
            .as_ref()
            .map_or_else(|| item.ident.to_string(), |(_, rename)| rename.to_string());
        self.uses.push(Named::Import {
            path: vec![item.ident.to_string()],
            alias: Some(alias),
            at,
        });
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        if let Some(first) = path.segments.first() {
            self.paths.push(Named::Path {
                segments: path
                    .segments
                    .iter()
                    .map(|segment| segment.ident.to_string())
                    .collect(),
                at: position(first.ident.span()),
            });
        }
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        visit::visit_macro(self, mac);
        self.tokens(mac.tokens.clone());
    }

    fn visit_meta_list(&mut self, list: &'ast syn::MetaList) {
        visit::visit_meta_list(self, list);
        self.tokens(list.tokens.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::{FORBIDDEN, check_source, python};

    /// The refused paths of `source`, as text.
    fn refused(source: &str) -> Vec<String> {
        check_source(source)
            .unwrap()
            .into_iter()
            .map(|violation| violation.path)
            .collect()
    }

    /// Every entry of the forbidden list is effective as a fully qualified path in an
    /// expression: a new entry that the matcher could not see would otherwise protect nothing.
    #[test]
    fn every_forbidden_path_is_refused_fully_qualified() {
        for (prefix, _) in FORBIDDEN {
            let path = prefix.join("::");
            let source = format!("fn probe() {{ let _ = {path}; }}");
            assert_eq!(
                refused(&source),
                vec![path.clone()],
                "{path} was not refused"
            );
        }
    }

    /// What pure policy legitimately uses (durations, the time type passed in as `now`,
    /// formatting, the crate's own modules) passes, so the check never pushes a decision out of
    /// `domain/` for a false reason.
    #[test]
    fn pure_code_passes() {
        let source = r#"
            use std::fmt;
            use std::time::Duration;
            use jiff::Timestamp;
            use crate::domain::ids::Id;
            enum Kind { Instant, Text }
            fn due(now: Timestamp, at: Timestamp) -> bool { at <= now }
            fn wait(kind: Kind) -> Duration {
                match kind { Kind::Instant => Duration::from_secs(1), Kind::Text => Duration::ZERO }
            }
            fn show(f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", Timestamp::MAX) }
            #[cfg(test)]
            mod tests { #[test] fn t() { assert_eq!(1, 1); } }
        "#;
        assert_eq!(refused(source), Vec::<String>::new());
    }

    /// Each way a path can be named is seen: a grouped and a renamed import, a fully qualified
    /// type with a leading `::`, an alias of a module, a macro's arguments, a derive, a nested
    /// module, `extern crate` and a glob. A form the check missed would let I/O into the policy
    /// unnoticed.
    #[test]
    fn every_way_of_naming_a_path_is_seen() {
        let cases = [
            ("use std::{env, fmt};", "std::env"),
            (
                "use std::time::SystemTime as Clock;",
                "std::time::SystemTime",
            ),
            ("fn f(pool: &::sqlx::PgPool) {}", "sqlx::PgPool"),
            (
                "use std::time; fn f() { let _ = time::Instant::now(); }",
                "std::time::Instant::now",
            ),
            (
                "use jiff::Timestamp; fn f() -> Timestamp { Timestamp::now() }",
                "jiff::Timestamp::now",
            ),
            (
                r#"fn f() -> String { format!("{:?}", std::env::var("X")) }"#,
                "std::env::var",
            ),
            ("#[derive(sqlx::Type)] struct S;", "sqlx::Type"),
            (
                "mod inner { fn f() { tokio::spawn(async {}); } }",
                "tokio::spawn",
            ),
            ("extern crate reqwest;", "reqwest"),
            ("use std::time::*;", "std::time::*"),
            ("use lettre::{self as mail};", "lettre"),
        ];
        for (source, expected) in cases {
            assert_eq!(refused(source), vec![expected.to_owned()], "{source}");
        }
    }

    /// A finding names the line and column of the path, so a report can be followed straight to
    /// the code.
    #[test]
    fn a_finding_has_its_position() {
        let violations =
            check_source("use std::fmt;\n\nfn f() {\n    std::env::args();\n}\n").unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!((violations[0].line, violations[0].column), (4, 5));
        assert_eq!(violations[0].reaches, "the process environment");
    }

    /// A tracked file is Python by its name or by its `#!` line, whatever the interpreter's path
    /// or flags; a script for another interpreter, a document about Python and an empty file are
    /// not. So no Python script enters the repository, with or without an extension.
    #[test]
    fn python_is_found_by_name_and_by_its_interpreter_line() {
        for (path, head) in [
            ("tools/audit.py", ""),
            ("sdks/python-other/audit.py", ""),
            ("scripts/audit", "#!/usr/bin/env python3\nprint('hi')\n"),
            ("scripts/audit", "#!/usr/bin/env -S python3 -u\n"),
            ("scripts/audit", "#!/usr/bin/python\n"),
        ] {
            assert!(python(path, head.as_bytes()).is_some(), "{path}: {head}");
        }
        for (path, head) in [
            ("sdks/python/src/norbelys/client.py", ""),
            ("sdks/python/tests/test_client.py", ""),
            (
                "scripts/dev-init.sh",
                "#!/usr/bin/env bash\n# no python here\n",
            ),
            ("docs/python.md", "# Why the repository has no python\n"),
            ("scripts/empty", ""),
            (
                "crates/server/src/lib.rs",
                "//! #!python is not an interpreter line\n",
            ),
        ] {
            assert_eq!(python(path, head.as_bytes()), None, "{path}");
        }
    }
}

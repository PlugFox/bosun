//! The static card lint of #5: what can be checked without running anything of the item's.
//!
//! - A draft card (no issue behind it) fails.
//! - The contract block parses ([`crate::contract`]).
//! - The verdict shell exists, and every program the verdict names resolves through it (D4 of
//!   #7: `sh -c`, with `sh` taken from the Git installation on Windows). Words that are
//!   expansions (`$CARGO`, `$(...)`, globs) cannot be resolved statically and are skipped.
//! - Every `blocked-by` target exists, and no card blocks itself.
//! - Every resource in `needs` is known.
//!
//! Three warnings, which leave the card claimable (#44 B):
//!
//! - the verdict's last pipeline step sends stdout to `/dev/null`, or
//! - the last pipeline ends in `tail -1` and no step before it is a test runner: either way the
//!   verdict passes with nothing on stdout worth quoting;
//! - the card leans on an issue by number (#139): a `#N` in its acceptance section (from a
//!   heading starting with `Acceptance` to the next heading of its level or higher), in its
//!   verdict, or right after `see`, `per` or `as in` anywhere in the body. The session cannot
//!   read GitHub, so a card says what it needs itself. Provenance lines (`Parent:`,
//!   `Part of`), code spans and fenced blocks, and references such as `owner/repo#3` or `&#39;`
//!   are not counted. One warning per number, in order of first appearance.
//!
//! "The verdict already passes on the base commit" is not here: it runs the verdict, and in
//! stage 1 runs at claim only (D7 of #7).

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::card::{Card, ItemId};
use crate::contract::{Contract, ContractError};
use crate::shell::{self, Step};

/// How bad a [`Finding`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Reported; the card stays claimable.
    Warning,
    /// The card goes to Needs you with the findings and is never claimed.
    Error,
}

/// What a finding is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Code {
    /// The card is a draft: there is no issue behind it.
    Draft,
    /// The contract block is missing or invalid.
    Contract(ContractError),
    /// The verdict shell cannot be started.
    ShellMissing,
    /// A program the verdict runs does not resolve through the verdict shell.
    ProgramMissing(String),
    /// A `blocked-by` target that does not exist.
    UnknownBlocker(ItemId),
    /// The card lists itself in `blocked-by`.
    BlocksItself,
    /// A resource in `needs` that the configuration does not declare.
    UnknownResource(String),
    /// The verdict's last step sends stdout to `/dev/null` (warning).
    StdoutDiscarded,
    /// The verdict ends in `tail -1` of something that is not a test runner (warning).
    TailOfNonRunner,
    /// The card leans on an issue by number, which the session cannot read (warning).
    UnquotedReference(u64),
}

/// One problem found by [`lint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Whether it blocks the claim.
    pub severity: Severity,
    /// What it is about.
    pub code: Code,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.severity {
            Severity::Error => f.write_str("error: ")?,
            Severity::Warning => f.write_str("warning: ")?,
        }
        match &self.code {
            Code::Draft => f.write_str("the card is a draft; convert it to an issue"),
            Code::Contract(e) => e.fmt(f),
            Code::ShellMissing => f.write_str("the verdict shell cannot be found"),
            Code::ProgramMissing(p) => {
                write!(f, "the verdict runs `{p}`, which the verdict shell cannot find")
            }
            Code::UnknownBlocker(item) => write!(f, "`blocked-by` names {item}, which does not exist"),
            Code::BlocksItself => f.write_str("`blocked-by` names the card itself"),
            Code::UnknownResource(r) => write!(f, "`needs` names `{r}`, which is not a configured resource"),
            Code::StdoutDiscarded => f.write_str(
                "the verdict's last step sends stdout to /dev/null: it passes with nothing to quote",
            ),
            Code::TailOfNonRunner => f.write_str(
                "the verdict ends in `tail -1` of a command that is not a test runner: its last \
                 line may not be the verdict's",
            ),
            Code::UnquotedReference(n) => write!(
                f,
                "the card leans on #{n}, which the session cannot read: quote what it needs \
                 from #{n} in the card"
            ),
        }
    }
}

/// The lint of one card.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lint {
    /// Every finding, errors and warnings, in check order.
    pub findings: Vec<Finding>,
    /// The contract, when the block parsed.
    pub contract: Option<Contract>,
}

impl Lint {
    /// Whether the card may be claimed: no finding is an error.
    #[must_use]
    pub fn claimable(&self) -> bool {
        self.findings.iter().all(|f| f.severity < Severity::Error)
    }

    fn error(&mut self, code: Code) {
        self.findings.push(Finding {
            severity: Severity::Error,
            code,
        });
    }

    fn warning(&mut self, code: Code) {
        self.findings.push(Finding {
            severity: Severity::Warning,
            code,
        });
    }
}

/// Resolves programs the way the verdict shell would. Injectable, so lint's tests do not depend
/// on what is installed; [`ShellResolver`] is the real one.
pub trait Resolve {
    /// Whether the verdict shell itself can be started.
    fn shell_available(&self) -> bool;
    /// Whether the verdict shell would find `program` (a name on its `PATH`, a builtin, or a
    /// path relative to the checkout).
    fn resolves(&self, program: &str) -> bool;
}

/// A [`Resolve`] from a closure, with the shell always available: for tests and callers that
/// already know the answer.
#[derive(Debug, Clone, Copy)]
pub struct FnResolver<F>(pub F);

impl<F: Fn(&str) -> bool> Resolve for FnResolver<F> {
    fn shell_available(&self) -> bool {
        true
    }

    fn resolves(&self, program: &str) -> bool {
        (self.0)(program)
    }
}

/// What lint checks a card against.
#[derive(Clone, Copy)]
pub struct LintContext<'a> {
    /// How programs resolve.
    pub resolver: &'a dyn Resolve,
    /// The resources the repository's configuration declares.
    pub resources: &'a BTreeSet<String>,
    /// Whether an item exists (on the board, or as an issue of the repository).
    pub item_exists: &'a dyn Fn(ItemId) -> bool,
}

impl fmt::Debug for LintContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LintContext")
            .field("resources", &self.resources)
            .finish_non_exhaustive()
    }
}

/// Lints `card`.
#[must_use]
pub fn lint(card: &Card, context: &LintContext<'_>) -> Lint {
    let mut lint = Lint::default();
    if cfg!(mutant_queue_lint_off) {
        // Mutant (#51): lint finds nothing, so the card with no verdict is claimable.
        return lint;
    }
    if card.draft {
        lint.error(Code::Draft);
    }
    let contract = match Contract::parse(&card.body) {
        Ok(contract) => contract,
        Err(errors) => {
            for e in errors {
                lint.error(Code::Contract(e));
            }
            return lint;
        }
    };

    if context.resolver.shell_available() {
        let analysis = shell::analyse(&contract.verdict);
        let mut seen = BTreeSet::new();
        for program in &analysis.programs {
            if seen.insert(program.as_str()) && !context.resolver.resolves(program) {
                lint.error(Code::ProgramMissing(program.clone()));
            }
        }
        if stdout_discarded(analysis.last_pipeline.last()) {
            lint.warning(Code::StdoutDiscarded);
        }
        if tail_of_non_runner(&analysis.last_pipeline) {
            lint.warning(Code::TailOfNonRunner);
        }
    } else {
        lint.error(Code::ShellMissing);
    }

    for &blocker in &contract.blocked_by {
        if blocker == card.id {
            lint.error(Code::BlocksItself);
        } else if !(context.item_exists)(blocker) {
            lint.error(Code::UnknownBlocker(blocker));
        }
    }
    for resource in &contract.needs {
        if !context.resources.contains(resource) {
            lint.error(Code::UnknownResource(resource.clone()));
        }
    }
    // Mutant (`mutant_queue_lint_references_unwarned`): no reference is ever warned about.
    if !cfg!(mutant_queue_lint_references_unwarned) {
        for n in leaned_on_references(&card.body, &contract.verdict) {
            lint.warning(Code::UnquotedReference(n));
        }
    }
    lint.contract = Some(contract);
    lint
}

fn stdout_discarded(step: Option<&Step>) -> bool {
    step.is_some_and(|step| {
        step.redirects.iter().any(|r| {
            let stdout = match r.op {
                ">" | ">>" | ">|" => matches!(r.fd, None | Some(1)),
                "&>" | "&>>" => true,
                _ => false,
            };
            stdout && r.target == "/dev/null"
        })
    })
}

/// Programs whose output ends in their own summary line.
const RUNNERS: &[&str] = &[
    "pytest", "py.test", "jest", "vitest", "mocha", "ctest", "rspec", "phpunit", "tox", "nox",
    "nextest",
];

/// Programs that run tests when an argument says `test`: `cargo test`, `go test ./...`,
/// `npm test`, `flutter test`.
const TEST_COMMANDS: &[&str] = &[
    "cargo", "go", "npm", "pnpm", "yarn", "bun", "deno", "dotnet", "flutter", "dart", "mix",
    "gradle", "gradlew", "mvn", "make", "swift", "zig", "python", "python3", "py", "uv",
];

fn is_test_runner(step: &Step) -> bool {
    let Some(name) = step.program_name() else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    RUNNERS.contains(&name.as_str())
        || (TEST_COMMANDS.contains(&name.as_str())
            && step
                .args()
                .any(|a| a.contains("test") || a.contains("nextest")))
}

fn tail_of_non_runner(steps: &[Step]) -> bool {
    let Some((last, before)) = steps.split_last() else {
        return false;
    };
    if before.is_empty() || last.program_name() != Some("tail") {
        return false;
    }
    let args: Vec<&str> = last.args().collect();
    let one_line = matches!(
        args[..],
        ["-1" | "-n1" | "--lines=1" | "-n+1"] | ["-n" | "--lines", "1"]
    );
    one_line && !before.iter().any(is_test_runner)
}

/// The issue numbers the card leans on, distinct, in order of first appearance: each `#N` of
/// the acceptance section, of the verdict, or right after `see`, `per` or `as in` in the body.
/// Provenance lines, code spans and fenced blocks are skipped; the verdict is read from
/// `verdict` only (in the body it sits in a fenced block).
fn leaned_on_references(body: &str, verdict: &str) -> Vec<u64> {
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    let mut push = |n: u64| {
        if seen.insert(n) {
            found.push(n);
        }
    };
    // The open fence: its character and length.
    let mut fence: Option<(char, usize)> = None;
    // The level of the acceptance heading, while inside its section.
    let mut acceptance: Option<usize> = None;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some((c, len)) = fence {
            let run = trimmed.chars().take_while(|&x| x == c).count();
            if run >= len && trimmed[run * c.len_utf8()..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if let Some(c) = trimmed.chars().next().filter(|&c| c == '`' || c == '~') {
            let run = trimmed.chars().take_while(|&x| x == c).count();
            if run >= 3 {
                fence = Some((c, run));
                continue;
            }
        }
        if let Some((level, text)) = heading(trimmed) {
            if acceptance.is_some_and(|open| level <= open) {
                acceptance = None;
            }
            if text
                .get(..10)
                .is_some_and(|start| start.eq_ignore_ascii_case("acceptance"))
            {
                acceptance = Some(level);
            }
        }
        // Mutant (`mutant_queue_lint_provenance_warns`): provenance lines are scanned too.
        if is_provenance(trimmed) && !cfg!(mutant_queue_lint_provenance_warns) {
            continue;
        }
        let text = without_code_spans(line);
        for (at, n) in references(&text) {
            if acceptance.is_some() || points_to(&text[..at]) {
                push(n);
            }
        }
    }
    for (_, n) in references(verdict) {
        push(n);
    }
    found
}

/// A Markdown ATX heading: its level and its text.
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|&c| c == '#').count();
    let rest = &line[level..];
    ((1..=6).contains(&level) && (rest.is_empty() || rest.starts_with([' ', '\t'])))
        .then(|| (level, rest.trim()))
}

/// Whether a line records where the card came from: `Parent:` or `Part of`, after an optional
/// list marker and optional `**`.
fn is_provenance(line: &str) -> bool {
    let line = line
        .strip_prefix(['-', '*', '+'])
        .map_or(line, str::trim_start);
    let line = line.strip_prefix("**").unwrap_or(line);
    let starts = |prefix: &str| {
        line.get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    };
    if starts("part of") {
        return true;
    }
    // Mutant (`mutant_queue_lint_parent_scanned`): a `Parent:` line is not provenance.
    !cfg!(mutant_queue_lint_parent_scanned) && starts("parent") && {
        let rest = &line["parent".len()..];
        rest.strip_prefix("**").unwrap_or(rest).starts_with(':')
    }
}

/// `line` with each inline code span replaced by a space. A run of backticks with no closing
/// run of the same length is text.
fn without_code_spans(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let run = after.len() - after.trim_start_matches('`').len();
        let body = &after[run..];
        let mut close = None;
        let mut at = 0;
        while let Some(found) = body[at..].find('`') {
            let from = at + found;
            let len = body[from..].len() - body[from..].trim_start_matches('`').len();
            if len == run {
                close = Some(from + len);
                break;
            }
            at = from + len;
        }
        if let Some(end) = close {
            out.push(' ');
            rest = &body[end..];
        } else {
            out.push_str(&after[..run]);
            rest = body;
        }
    }
    out.push_str(rest);
    out
}

/// Every `#N` of `text` that names an issue: its byte offset and number. Not one preceded by a
/// letter, digit, `_`, `/`, `&` or `#` (`owner/repo#3`, `&#39;`, `##`), nor digits followed by
/// a letter.
fn references(text: &str) -> Vec<(usize, u64)> {
    let mut out = Vec::new();
    for (at, _) in text.match_indices('#') {
        let before = text[..at].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '/' | '&' | '#')) {
            continue;
        }
        let digits = &text[at + 1..];
        let len = digits.len()
            - digits
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .len();
        if len == 0 || digits[len..].starts_with(char::is_alphabetic) {
            continue;
        }
        if let Ok(n) = digits[..len].parse() {
            out.push((at, n));
        }
    }
    out
}

/// Whether `before` (the text in front of a `#N`) ends in `see`, `per` or `as in`, then white
/// space and an optional `:`.
fn points_to(before: &str) -> bool {
    let t = before.trim_end();
    let t = t.strip_suffix(':').map_or(t, str::trim_end);
    ends_with_word(t, "see").is_some()
        || ends_with_word(t, "per").is_some()
        || ends_with_word(t, "in").is_some_and(|rest| {
            rest.ends_with(char::is_whitespace) && ends_with_word(rest.trim_end(), "as").is_some()
        })
}

/// `text` without `word` at its end (any case, a whole word), or `None`.
fn ends_with_word<'t>(text: &'t str, word: &str) -> Option<&'t str> {
    let cut = text.len().checked_sub(word.len())?;
    let rest = text.get(..cut)?;
    let whole = !rest
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_');
    (whole && text[cut..].eq_ignore_ascii_case(word)).then_some(rest)
}

/// The real [`Resolve`]: asks the verdict shell itself, `sh -c 'command -v -- "$1"'`, in the
/// repository's checkout. A builtin, a function of the shell's profile or a program on its
/// `PATH` resolves; so does a path, relative to the checkout, to an executable file.
#[derive(Debug, Clone)]
pub struct ShellResolver {
    shell: PathBuf,
    cwd: PathBuf,
}

impl ShellResolver {
    /// A resolver running `shell` in `cwd`.
    #[must_use]
    pub fn new(shell: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            shell: shell.into(),
            cwd: cwd.into(),
        }
    }

    /// The shell this resolver runs.
    #[must_use]
    pub fn shell(&self) -> &Path {
        &self.shell
    }

    fn run(&self, script: &str, arg: &str) -> bool {
        crate::child::command(&self.shell)
            .arg("-c")
            .arg(script)
            .arg("sh")
            .arg(arg)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

impl Resolve for ShellResolver {
    fn shell_available(&self) -> bool {
        self.run("exit 0", "")
    }

    fn resolves(&self, program: &str) -> bool {
        self.run("command -v -- \"$1\" >/dev/null 2>&1", program)
    }
}

/// The verdict shell (D4 of #7): `configured` when given (`repo.<name>.verdict.shell`),
/// otherwise `sh` from the Git installation on Windows, otherwise `sh` on `PATH`. `None` when
/// nothing is found; lint then reports [`Code::ShellMissing`].
#[must_use]
pub fn verdict_shell(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return Some(path.to_owned());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    if cfg!(windows) {
        git_sh(&dirs)
    } else {
        dirs.iter()
            .map(|d| d.join("sh"))
            .find(|p| p.is_file())
            .or_else(|| Some(PathBuf::from("/bin/sh")).filter(|p| p.is_file()))
    }
}

/// `sh.exe` of the Git for Windows installation that provides `git.exe` on `dirs`, else of the
/// default installation directory. Not `sh.exe` from `PATH` itself: that may be another
/// toolchain's (MSYS2, Cygwin, a WSL launcher).
fn git_sh(dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = dirs
        .iter()
        .filter(|d| d.join("git.exe").is_file())
        .filter_map(|d| {
            // `<root>\cmd`, `<root>\bin` or `<root>\mingw64\bin`.
            let parent = d.parent()?;
            if parent
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("mingw64"))
            {
                parent.parent().map(Path::to_owned)
            } else {
                Some(parent.to_owned())
            }
        })
        .collect();
    for var in ["ProgramFiles", "ProgramW6432", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            let base = PathBuf::from(base);
            roots.push(base.join("Git"));
            roots.push(base.join("Programs").join("Git"));
        }
    }
    roots
        .iter()
        .flat_map(|root| {
            [
                root.join("bin").join("sh.exe"),
                root.join("usr").join("bin").join("sh.exe"),
            ]
        })
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Column;

    fn card(id: u64, verdict: Option<&str>, extra: &str) -> Card {
        let block = verdict.map_or_else(String::new, |v| format!("verdict = {v:?}\n"));
        Card {
            id: ItemId(id),
            title: "t".into(),
            column: Column::Ready,
            body: format!("Prose.\n\n```bosun\n{block}{extra}```\n"),
            labels: Vec::new(),
            draft: false,
            closed: false,
            comments: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    /// A card whose body is `prose`, then the block with `verdict`.
    fn card_with_body(prose: &str, verdict: &str) -> Card {
        Card {
            body: format!("{prose}\n\n```bosun\nverdict = {verdict:?}\n```\n"),
            ..card(1, Some(verdict), "")
        }
    }

    const INSTALLED: &[&str] = &["cargo", "grep", "tail", "sh", "adb", "echo"];

    fn run(card: &Card) -> Lint {
        let resolver = FnResolver(|p: &str| INSTALLED.contains(&p));
        let resources = BTreeSet::from(["phone".to_owned()]);
        let exists = |item: ItemId| item.0 < 100;
        lint(
            card,
            &LintContext {
                resolver: &resolver,
                resources: &resources,
                item_exists: &exists,
            },
        )
    }

    fn codes(lint: &Lint) -> Vec<&Code> {
        lint.findings.iter().map(|f| &f.code).collect()
    }

    #[test]
    fn a_card_with_no_verdict_is_not_claimable() {
        let lint = run(&card(1, None, "needs = [\"phone\"]\n"));
        assert_eq!(
            codes(&lint),
            [&Code::Contract(ContractError::MissingVerdict)]
        );
        assert!(!lint.claimable());
        assert!(lint.contract.is_none());
    }

    #[test]
    fn a_verdict_program_that_does_not_exist_is_not_claimable() {
        let lint = run(&card(1, Some("cargo test && frobnicate --check"), ""));
        assert_eq!(codes(&lint), [&Code::ProgramMissing("frobnicate".into())]);
        assert!(!lint.claimable());
    }

    #[test]
    fn a_function_the_verdict_defines_is_not_a_missing_program() {
        // Defined, then called: the shell runs the function, not a program on its PATH.
        let defined = r#"n() { grep -c x "$d"; }; b=$(n); n && echo "$b""#;
        let lint = run(&card(1, Some(defined), ""));
        assert_eq!(lint.findings, [], "{defined}");
        assert!(lint.claimable());
        // Called before its definition, or never defined: a missing program.
        let undefined = r#"m; n() { grep -c x "$d"; }; n && m"#;
        let lint = run(&card(1, Some(undefined), ""));
        assert_eq!(codes(&lint), [&Code::ProgramMissing("m".into())]);
        assert!(!lint.claimable());
        let lint = run(&card(1, Some("n() { echo; }; m"), ""));
        assert_eq!(codes(&lint), [&Code::ProgramMissing("m".into())]);
    }

    #[test]
    fn an_unknown_effort_level_is_refused() {
        let lint = run(&card(1, Some("cargo test"), "effort = \"ultra\"\n"));
        assert!(!lint.claimable());
        let [finding] = &lint.findings[..] else {
            panic!("{:?}", lint.findings);
        };
        assert!(matches!(
            &finding.code,
            Code::Contract(ContractError::Field { key, .. }) if key == "effort"
        ));
        let shown = finding.to_string();
        assert!(
            shown.contains("effort") && shown.contains("ultra"),
            "{shown}"
        );
        for level in bosun_core::EFFORT_LEVELS {
            assert!(shown.contains(level), "{shown}");
        }
        for level in bosun_core::EFFORT_LEVELS {
            let lint = run(&card(
                1,
                Some("cargo test"),
                &format!("effort = {level:?}\n"),
            ));
            assert_eq!(lint.findings, [], "{level}");
            assert!(lint.claimable(), "{level}");
            assert_eq!(lint.contract.unwrap().effort.as_deref(), Some(level));
        }
    }

    #[test]
    fn a_clean_card_is_claimable() {
        let lint = run(&card(
            2,
            Some("RUST_LOG=warn cargo test -p a 2>&1 | grep -q 'test result: ok'"),
            "needs = [\"phone\"]\nblocked-by = [\"#1\"]\n",
        ));
        assert_eq!(lint.findings, []);
        assert!(lint.claimable());
        assert_eq!(lint.contract.unwrap().blocked_by, [ItemId(1)]);
    }

    #[test]
    fn a_missing_shell_is_an_error() {
        struct NoShell;
        impl Resolve for NoShell {
            fn shell_available(&self) -> bool {
                false
            }
            fn resolves(&self, _: &str) -> bool {
                true
            }
        }
        let resources = BTreeSet::new();
        let lint = lint(
            &card(1, Some("cargo test"), ""),
            &LintContext {
                resolver: &NoShell,
                resources: &resources,
                item_exists: &|_| true,
            },
        );
        assert_eq!(codes(&lint), [&Code::ShellMissing]);
    }

    #[test]
    fn blockers_resources_and_drafts() {
        let mut draft = card(
            5,
            Some("cargo test"),
            "blocked-by = [5, \"#100\"]\nneeds = [\"gpu\"]\n",
        );
        draft.draft = true;
        let lint = run(&draft);
        assert_eq!(
            codes(&lint),
            [
                &Code::Draft,
                &Code::BlocksItself,
                &Code::UnknownBlocker(ItemId(100)),
                &Code::UnknownResource("gpu".into()),
            ]
        );
        assert!(lint.findings.iter().all(|f| !f.to_string().is_empty()));
    }

    #[test]
    fn a_dev_null_verdict_warns_and_stays_claimable() {
        for verdict in [
            "cargo test >/dev/null",
            "cargo test 2>&1 | grep -q ok 1>/dev/null",
            "cargo test &>/dev/null",
        ] {
            let lint = run(&card(1, Some(verdict), ""));
            assert_eq!(codes(&lint), [&Code::StdoutDiscarded], "{verdict}");
            assert_eq!(lint.findings[0].severity, Severity::Warning);
            assert!(lint.claimable(), "{verdict}");
        }
        // stderr alone to /dev/null leaves stdout to quote.
        assert_eq!(
            run(&card(1, Some("cargo test 2>/dev/null"), "")).findings,
            []
        );
    }

    #[test]
    fn a_tail_1_verdict_warns_and_stays_claimable() {
        for verdict in [
            "adb logcat -d | grep frob | tail -1",
            "adb logcat -d | tail -n 1",
        ] {
            let lint = run(&card(1, Some(verdict), ""));
            assert_eq!(codes(&lint), [&Code::TailOfNonRunner], "{verdict}");
            assert!(lint.claimable(), "{verdict}");
        }
        // A test runner's last line is its summary.
        for verdict in [
            "cargo test 2>&1 | tail -1",
            "adb logcat | tail -5",
            "tail -1 log",
        ] {
            assert_eq!(run(&card(1, Some(verdict), "")).findings, [], "{verdict}");
        }
    }

    #[test]
    fn a_bare_issue_reference_in_the_acceptance_or_the_verdict_warns() {
        for (prose, verdict, n) in [
            (
                "Intro.\n\n## Acceptance\n\n- [ ] Behaves as decided in #8.\n",
                "cargo test",
                8,
            ),
            ("Prose.", "cargo test -p demo # #12", 12),
            ("Keep the old format, see #9.", "cargo test", 9),
            ("Name it per #10.", "cargo test", 10),
            ("As in #11, the flag is hidden.", "cargo test", 11),
            (
                "## Acceptance\n\n- [ ] Behaves as #8 says.\n- [ ] And keeps #8.\n",
                "cargo test",
                8,
            ),
        ] {
            let lint = run(&card_with_body(prose, verdict));
            assert_eq!(codes(&lint), [&Code::UnquotedReference(n)], "{prose}");
            assert_eq!(lint.findings[0].severity, Severity::Warning);
            assert!(lint.findings[0].to_string().contains(&format!("#{n}")));
            assert!(lint.claimable(), "{prose}");
        }
        // Distinct numbers, in order of first appearance.
        let lint = run(&card_with_body(
            "See: #3.\n\n### Acceptance criteria\n\n- [ ] #2, #3 and #1.\n",
            "cargo test",
        ));
        assert_eq!(
            codes(&lint),
            [
                &Code::UnquotedReference(3),
                &Code::UnquotedReference(2),
                &Code::UnquotedReference(1),
            ]
        );
    }

    #[test]
    fn provenance_lines_and_references_outside_the_acceptance_do_not_warn() {
        let prose = "Parent: #8 (the decisions quoted below)\n\n\
                     The bug, which #5 first saw, is quoted here.\n\n\
                     ## Acceptance\n\n\
                     Part of #7 (found in a run).\n\
                     - **Part of** #7\n\
                     Parent: #9, see #9 there.\n\
                     - **Parent:** #9\n\
                     - [ ] The fix keeps `see #4` working.\n\
                     - [ ] owner/repo#3 is untouched; so is &#39; and #4a.\n\n\
                     ```text\n#2 inside a fence\n```\n\n\
                     ### Mutants\n\n- [ ] `mutant_demo`\n\n\
                     ## Notes\n\n\
                     #6 was looked at too.\n";
        let lint = run(&card_with_body(prose, "cargo test"));
        assert_eq!(lint.findings, [], "{prose}");
    }

    #[test]
    fn the_real_resolver_asks_the_shell() {
        let shell = verdict_shell(None).expect("a POSIX shell (Git's on Windows) is installed");
        let dir = tempfile::tempdir().unwrap();
        let resolver = ShellResolver::new(shell, dir.path());
        assert!(resolver.shell_available());
        assert!(resolver.resolves("cd"), "a builtin");
        assert!(resolver.resolves("ls"), "a program on the shell's PATH");
        assert!(!resolver.resolves("bosun-no-such-program-7f3a"));
        assert!(!resolver.resolves("./missing.sh"));
        let missing = ShellResolver::new(dir.path().join("no-shell"), dir.path());
        assert!(!missing.shell_available());
    }

    #[test]
    fn a_configured_shell_wins() {
        let configured = Path::new("/opt/custom/sh");
        assert_eq!(verdict_shell(Some(configured)).as_deref(), Some(configured));
    }
}

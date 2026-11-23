//! Shell commands as file touches (#44 A, #76).
//!
//! A `Bash` or `PowerShell` tool call reads and writes files as much as the file tools do, so
//! its command is classified, per pipeline segment (`|`, `;`, `&&`, `||`, `&`, a newline, a
//! subshell, `$(...)`, backticks, `<(...)`), into the files it names and how:
//!
//! - a read verb (`cat head tail grep rg wc diff jq nl od less`, `sed` without `-i`, and in
//!   PowerShell `Get-Content`, `Select-String` and their aliases) reads its file arguments; an
//!   input redirect (`< file`) reads its file;
//! - a write verb (`sed -i`, `tee`, `truncate`, `patch`, and in PowerShell `Set-Content`,
//!   `Add-Content`, `Out-File`) writes its file arguments; an output redirect (`>`, `>>`,
//!   `2>`, `&>`) writes its file;
//! - the script argument of an interpreter (`python`, `py`, `bash`, `sh`, `node`, `pwsh`,
//!   `source`, through `uv run` and `mise exec -- ...`), or a script run as the command
//!   itself (`./x.sh`, `.\x.ps1`), is [`TouchKind::Exec`]: running a script is not reading it;
//! - every other verb is unknown, and a file it names is [`TouchKind::Unknown`]: a gap, never
//!   a read.
//!
//! The same file may be named twice with two kinds (`cat x.py && python x.py` is one read and
//! one exec).
//!
//! What counts as a file is decided from the text alone, without the file system: a literal
//! word (no variable, substitution or glob) of safe characters whose last component has an
//! extension with a letter (`src/lib.rs`, `Cargo.toml`) or is a dot-file (`.gitignore`). An
//! interpreter's script argument needs no extension. Everything else -- flags, patterns,
//! messages, URLs, numbers -- is dropped here, so it never leaves this module; the store then
//! keeps only the files inside the message's repository (#2). Known gaps: a file without an
//! extension and outside a script position (`Makefile`), a path with spaces, and a path held
//! in a variable are not seen.

use bosun_core::TouchKind;

/// The shell a command is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// POSIX shells (`sh`, `bash`), as Claude Code's `Bash` tool runs them.
    Posix,
    /// PowerShell, as Claude Code's `PowerShell` tool runs it.
    PowerShell,
}

/// The files `command` names, as written (relative paths are relative to the directory the
/// command started in), with how it touches each, in order of appearance. A `cd` earlier in
/// the command moves the relative paths after it; after a `cd` to a place that cannot be told,
/// relative paths are dropped.
#[must_use]
pub(crate) fn touches(command: &str, dialect: Dialect) -> Vec<(String, TouchKind)> {
    let mut out = Out {
        dir: Dir::Start,
        touches: Vec::new(),
    };
    classify(command, dialect, 0, &mut out);
    out.touches
}

/// How deep substitutions and `bash -c` strings are followed.
const MAX_DEPTH: usize = 8;

/// The kind of a script argument.
fn exec() -> TouchKind {
    // Mutant `mutant_ingest_exec_is_read`: running a script reads it.
    if cfg!(mutant_ingest_exec_is_read) {
        TouchKind::Read
    } else {
        TouchKind::Exec
    }
}

/// The kind of a file named by an unknown verb.
fn unknown() -> TouchKind {
    // Mutant `mutant_ingest_unknown_is_read`: an unknown verb reads its files.
    if cfg!(mutant_ingest_unknown_is_read) {
        TouchKind::Read
    } else {
        TouchKind::Unknown
    }
}

/// Where relative paths are, after the `cd`s seen so far.
#[derive(Debug, Clone)]
enum Dir {
    /// The directory the command started in.
    Start,
    /// A path, relative to the start or absolute.
    At(String),
    /// Somewhere that cannot be told from the text.
    Lost,
}

#[derive(Debug)]
struct Out {
    dir: Dir,
    touches: Vec<(String, TouchKind)>,
}

impl Out {
    /// Adds `word` as a file of `kind` when it looks like one ([`path_like`]).
    fn file(&mut self, word: &Word, kind: TouchKind) {
        if word.literal && path_like(&word.text) {
            self.push(&word.text, kind);
        }
    }

    /// Adds `word` as a script: any literal word of safe characters.
    fn script(&mut self, word: &Word) {
        if word.literal && plain(&word.text) {
            self.push(&word.text, exec());
        }
    }

    fn push(&mut self, path: &str, kind: TouchKind) {
        let path = match &self.dir {
            _ if is_absolute(path) => path.to_owned(),
            Dir::Start => path.to_owned(),
            Dir::At(dir) => format!("{dir}/{path}"),
            Dir::Lost => return,
        };
        self.touches.push((path, kind));
    }

    /// Follows `cd target` (`None`: a `cd` whose target cannot be told).
    fn cd(&mut self, target: Option<&Word>) {
        self.dir = match (target, &self.dir) {
            (Some(word), _) if word.literal && plain(&word.text) && is_absolute(&word.text) => {
                Dir::At(word.text.clone())
            }
            (Some(word), Dir::Start) if word.literal && plain(&word.text) => {
                Dir::At(word.text.clone())
            }
            (Some(word), Dir::At(dir)) if word.literal && plain(&word.text) => {
                Dir::At(format!("{dir}/{}", word.text))
            }
            _ => Dir::Lost,
        };
    }
}

fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with(['/', '\\'])
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// A word of a command, unquoted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Word {
    text: String,
    /// No variable, substitution, glob or tilde: the text is the word the command sees.
    literal: bool,
}

/// One simple command: its words, the files of its redirects, and the commands substituted
/// into it.
#[derive(Debug, Default)]
struct Segment {
    words: Vec<Word>,
    redirects: Vec<(TouchKind, Word)>,
    nested: Vec<String>,
}

/// What the next word is, after a redirect operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// A file of this kind.
    File(TouchKind),
    /// A file written, unless it is a descriptor (`>&2`, `>&-`).
    Duplicate,
    /// A here-document delimiter; `true` for `<<-`, which strips leading tabs.
    HereDoc(bool),
    /// Not a file (a here-string).
    Drop,
}

fn classify(text: &str, dialect: Dialect, depth: usize, out: &mut Out) {
    if depth > MAX_DEPTH {
        return;
    }
    for segment in Lexer::new(text, dialect).run() {
        for nested in &segment.nested {
            classify(nested, dialect, depth + 1, out);
        }
        for (kind, word) in &segment.redirects {
            out.file(word, *kind);
        }
        command(&segment.words, dialect, depth, out);
    }
}

struct Lexer {
    chars: Vec<char>,
    i: usize,
    dialect: Dialect,
    segments: Vec<Segment>,
    current: Segment,
    word: Option<Word>,
    pending: Option<Pending>,
    /// Here-documents whose bodies start at the next newline: delimiter, strip tabs.
    heredocs: Vec<(String, bool)>,
}

impl Lexer {
    fn new(text: &str, dialect: Dialect) -> Self {
        Self {
            chars: text.chars().collect(),
            i: 0,
            dialect,
            segments: Vec::new(),
            current: Segment::default(),
            word: None,
            pending: None,
            heredocs: Vec::new(),
        }
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.i + ahead).copied()
    }

    fn word(&mut self) -> &mut Word {
        self.word.get_or_insert_with(|| Word {
            text: String::new(),
            literal: true,
        })
    }

    fn push(&mut self, c: char) {
        self.word().text.push(c);
    }

    fn taint(&mut self) {
        self.word().literal = false;
    }

    fn end_word(&mut self) {
        let Some(word) = self.word.take() else {
            return;
        };
        match self.pending.take() {
            None => self.current.words.push(word),
            Some(Pending::File(kind)) => self.current.redirects.push((kind, word)),
            Some(Pending::Duplicate) => {
                if !(word.text == "-" || word.text.chars().all(|c| c.is_ascii_digit())) {
                    self.current.redirects.push((TouchKind::Write, word));
                }
            }
            Some(Pending::HereDoc(strip)) => self.heredocs.push((word.text, strip)),
            Some(Pending::Drop) => {}
        }
    }

    fn end_segment(&mut self) {
        self.end_word();
        self.pending = None;
        let segment = std::mem::take(&mut self.current);
        if !(segment.words.is_empty() && segment.redirects.is_empty() && segment.nested.is_empty())
        {
            self.segments.push(segment);
        }
    }

    /// Starts a redirect: a word made only of digits just before it is a descriptor.
    fn redirect(&mut self, pending: Pending) {
        let descriptor = self.word.as_ref().is_some_and(|w| {
            w.literal
                && !w.text.is_empty()
                && w.text.chars().all(|c| c.is_ascii_digit() || c == '*')
        });
        if descriptor {
            self.word = None;
        } else {
            self.end_word();
        }
        self.pending = Some(pending);
    }

    /// The index of the `)` that closes the `(` at `open`, or the end of the text.
    fn close(&self, open: usize) -> usize {
        let mut depth = 0usize;
        let mut j = open;
        while j < self.chars.len() {
            match self.chars[j] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return j;
                    }
                }
                '\'' => {
                    j += 1;
                    while j < self.chars.len() && self.chars[j] != '\'' {
                        j += 1;
                    }
                }
                '"' => {
                    j += 1;
                    while j < self.chars.len() && self.chars[j] != '"' {
                        if self.chars[j] == '\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        self.chars.len()
    }

    /// A substitution `(...)` whose `(` is at `open`: its text is classified on its own.
    fn substitute(&mut self, open: usize) {
        let end = self.close(open);
        let inner: String = self.chars[(open + 1).min(end)..end].iter().collect();
        self.current.nested.push(inner);
        self.i = end + 1;
    }

    /// `$` at `self.i`: a substitution, a parameter or a variable. The word is not literal.
    fn dollar(&mut self) {
        self.taint();
        match self.peek(1) {
            Some('(') if self.peek(2) == Some('(') => {
                // Arithmetic: nothing to classify.
                self.i = self.close(self.i + 1) + 1;
            }
            Some('(') => self.substitute(self.i + 1),
            Some('{') => {
                while self.i < self.chars.len() && self.chars[self.i] != '}' {
                    self.i += 1;
                }
                self.i += 1;
            }
            _ => {
                self.push('$');
                self.i += 1;
            }
        }
    }

    /// The bodies of pending here-documents, from the start of a line.
    fn skip_heredocs(&mut self) {
        for (delimiter, strip) in std::mem::take(&mut self.heredocs) {
            while self.i < self.chars.len() {
                let start = self.i;
                while self.i < self.chars.len() && self.chars[self.i] != '\n' {
                    self.i += 1;
                }
                let line: String = self.chars[start..self.i].iter().collect();
                self.i += 1;
                let line = line.trim_end_matches('\r');
                let line = if strip {
                    line.trim_start_matches('\t')
                } else {
                    line
                };
                if line == delimiter {
                    break;
                }
            }
        }
    }

    /// A PowerShell here-string `@'...'@` or `@"..."@` at `self.i`: skipped whole.
    fn here_string(&mut self) -> bool {
        let Some(quote) = self.peek(1).filter(|q| matches!(q, '\'' | '"')) else {
            return false;
        };
        let rest: String = self.chars[self.i + 2..].iter().collect();
        let Some(newline) = rest.find('\n') else {
            return false;
        };
        if !rest[..newline].trim().is_empty() {
            return false;
        }
        let close = format!("\n{quote}@");
        let end = rest.find(&close).map_or(rest.len(), |at| at + close.len());
        self.taint();
        self.i += 2 + rest[..end].chars().count();
        true
    }

    fn single_quoted(&mut self) {
        self.word();
        self.i += 1;
        while let Some(c) = self.peek(0) {
            self.i += 1;
            if c == '\'' {
                // PowerShell doubles a quote inside single quotes.
                if self.dialect == Dialect::PowerShell && self.peek(0) == Some('\'') {
                    self.push('\'');
                    self.i += 1;
                    continue;
                }
                return;
            }
            self.push(c);
        }
    }

    fn double_quoted(&mut self) {
        self.word();
        self.i += 1;
        while let Some(c) = self.peek(0) {
            match (c, self.dialect) {
                ('"', _) => {
                    self.i += 1;
                    return;
                }
                ('\\', Dialect::Posix) if matches!(self.peek(1), Some('"' | '\\' | '$' | '`')) => {
                    let next = self.peek(1).unwrap_or_default();
                    self.push(next);
                    self.i += 2;
                }
                ('\\', Dialect::Posix) if self.peek(1) == Some('\n') => self.i += 2,
                ('`', Dialect::PowerShell) => {
                    if let Some(next) = self.peek(1) {
                        self.push(next);
                    }
                    self.i += 2;
                }
                ('`', Dialect::Posix) => self.backtick(),
                ('$', _) => self.dollar(),
                _ => {
                    self.push(c);
                    self.i += 1;
                }
            }
        }
    }

    /// A POSIX backtick substitution at `self.i`.
    fn backtick(&mut self) {
        self.taint();
        let start = self.i + 1;
        let mut j = start;
        while j < self.chars.len() && self.chars[j] != '`' {
            if self.chars[j] == '\\' {
                j += 1;
            }
            j += 1;
        }
        let end = j.min(self.chars.len());
        self.current
            .nested
            .push(self.chars[start..end].iter().collect());
        self.i = end + 1;
    }

    #[allow(clippy::too_many_lines)]
    fn run(mut self) -> Vec<Segment> {
        let posix = self.dialect == Dialect::Posix;
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' | '\r' => {
                    self.end_word();
                    self.i += 1;
                }
                '\n' => {
                    self.end_segment();
                    self.i += 1;
                    self.skip_heredocs();
                }
                '#' if self.word.is_none() => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                '|' => {
                    self.end_segment();
                    self.i += if matches!(self.peek(1), Some('|' | '&')) {
                        2
                    } else {
                        1
                    };
                }
                ';' | '(' | ')' => {
                    self.end_segment();
                    self.i += 1;
                }
                '{' | '}' if !posix => {
                    self.end_segment();
                    self.i += 1;
                }
                '&' if self.peek(1) == Some('&') => {
                    self.end_segment();
                    self.i += 2;
                }
                '&' if posix && self.peek(1) == Some('>') => {
                    self.end_word();
                    self.i += if self.peek(2) == Some('>') { 3 } else { 2 };
                    self.pending = Some(Pending::File(TouchKind::Write));
                }
                '&' if posix => {
                    self.end_segment();
                    self.i += 1;
                }
                '&' if self.word.is_none() => {
                    // PowerShell's call operator: a word of its own.
                    self.push('&');
                    self.end_word();
                    self.i += 1;
                }
                '<' | '>' if self.peek(1) == Some('(') && posix => {
                    // Process substitution.
                    self.end_word();
                    self.substitute(self.i + 1);
                }
                '>' => {
                    self.i += 1;
                    if matches!(self.peek(0), Some('>' | '|')) {
                        self.i += 1;
                    }
                    if self.peek(0) == Some('&') {
                        self.i += 1;
                        self.redirect(Pending::Duplicate);
                    } else {
                        self.redirect(Pending::File(TouchKind::Write));
                    }
                }
                '<' => {
                    self.i += 1;
                    let pending = match (self.peek(0), self.peek(1)) {
                        (Some('<'), Some('<')) => {
                            self.i += 2;
                            Pending::Drop
                        }
                        (Some('<'), Some('-')) => {
                            self.i += 2;
                            Pending::HereDoc(true)
                        }
                        (Some('<'), _) => {
                            self.i += 1;
                            Pending::HereDoc(false)
                        }
                        (Some('&'), _) => {
                            self.i += 1;
                            Pending::Drop
                        }
                        _ => Pending::File(TouchKind::Read),
                    };
                    self.redirect(pending);
                }
                '\'' => self.single_quoted(),
                '"' => self.double_quoted(),
                '\\' if posix => {
                    // An escape before a special character, a line continuation before a
                    // newline, and otherwise a literal backslash (a Windows path).
                    const SPECIAL: &str = " \t\"'\\$`|&;()<>*?[]#~{}";
                    match self.peek(1) {
                        Some('\n') => self.i += 2,
                        Some(next) if SPECIAL.contains(next) => {
                            self.push(next);
                            self.i += 2;
                        }
                        _ => {
                            self.push('\\');
                            self.i += 1;
                        }
                    }
                }
                '`' if posix => self.backtick(),
                '`' => {
                    // PowerShell's escape; a line continuation before a newline.
                    match self.peek(1) {
                        Some('\n') | None => {}
                        Some(next) => self.push(next),
                    }
                    self.i += 2;
                }
                '$' => self.dollar(),
                '@' if !posix && self.word.is_none() && self.here_string() => {}
                '*' | '?' | '[' => {
                    self.push(c);
                    self.taint();
                    self.i += 1;
                }
                '~' if self.word.is_none() => {
                    self.push(c);
                    self.taint();
                    self.i += 1;
                }
                _ => {
                    self.push(c);
                    self.i += 1;
                }
            }
        }
        self.end_segment();
        self.segments
    }
}

/// Words that run the rest of the command, or start a clause, and name no file themselves.
const PREFIXES: &[&str] = &[
    "!", "{", "}", "if", "then", "else", "elif", "while", "until", "do", "time", "exec", "command",
    "builtin", "nohup", "env", "sudo", "nice", "xargs", "&", "timeout",
];

/// Verbs whose arguments are never files.
const SILENT: &[&str] = &[
    "echo",
    "printf",
    "export",
    "set",
    "unset",
    "alias",
    "sleep",
    "exit",
    "return",
    "true",
    "false",
    "which",
    "write-host",
    "write-output",
    "start-sleep",
];

/// A number or a duration: `10`, `1.5`, `30s`.
fn is_count(word: &Word) -> bool {
    let digits = word.text.trim_end_matches(['s', 'm', 'h', 'd']);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// `NAME=value`, as in `NAME=value command`.
fn is_assignment(word: &Word) -> bool {
    word.text.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The name a verb is matched by: its last path component, lower case, without `.exe`, and
/// `python3.12` as `python`.
fn verb_name(text: &str) -> String {
    let base = text
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(text)
        .to_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base).to_owned();
    match base.strip_prefix("python") {
        Some(version) if version.chars().all(|c| c.is_ascii_digit() || c == '.') => {
            "python".to_owned()
        }
        _ => base,
    }
}

/// The extensions of a script run as a command itself.
const SCRIPTS: &[&str] = &[
    "py", "sh", "bash", "ps1", "js", "mjs", "cjs", "ts", "rb", "pl", "bat", "cmd",
];

fn is_script(word: &Word) -> bool {
    word.literal
        && path_like(&word.text)
        && extension(&word.text).is_some_and(|ext| SCRIPTS.contains(&ext.to_lowercase().as_str()))
}

fn last_component(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn extension(path: &str) -> Option<&str> {
    let name = last_component(path);
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then_some(ext)
}

/// Safe characters only, not a flag, with a letter: what a script argument must be.
fn plain(text: &str) -> bool {
    let mut chars = text.char_indices();
    let drive = matches!(
        (chars.next(), chars.next()),
        (Some((_, letter)), Some((_, ':'))) if letter.is_ascii_alphabetic()
    );
    !text.is_empty()
        && text.len() <= 1024
        && !text.starts_with('-')
        && text.chars().any(char::is_alphabetic)
        && text.char_indices().all(|(i, c)| {
            (c.is_alphanumeric() || "/\\._-+".contains(c)) || (c == ':' && i == 1 && drive)
        })
}

/// Whether `text` looks like a file: [`plain`], and its last component has an extension
/// with a letter (`lib.rs`, `Cargo.toml`) or is a dot-file (`.gitignore`).
fn path_like(text: &str) -> bool {
    if !plain(text) {
        return false;
    }
    let name = last_component(text);
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if let Some(dotfile) = name.strip_prefix('.')
        && !dotfile.contains('.')
    {
        return dotfile.chars().any(char::is_alphabetic);
    }
    extension(text).is_some_and(|ext| {
        (1..=16).contains(&ext.len())
            && ext.chars().any(|c| c.is_ascii_alphabetic())
            && ext
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

/// A word is a flag: `-x`, `--name`, `-Name` (but not `-` alone).
fn is_flag(word: &Word) -> bool {
    word.text.len() > 1 && word.text.starts_with('-')
}

/// What a flag's value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value {
    /// Not a file (a count, an encoding, a glob).
    Skip,
    /// The pattern or script of a pattern-first verb: the first positional is then a file.
    Pattern,
    /// A file read, holding the pattern or script.
    PatternFile,
    /// A file, touched as the verb touches its arguments.
    Path,
    /// A file written.
    Output,
}

/// A read or write verb.
#[derive(Debug)]
struct Verb {
    names: &'static [&'static str],
    kind: TouchKind,
    /// The first positional is a pattern or script, unless a flag gave one.
    pattern_first: bool,
    /// Flags that take a value, and what the value is.
    flags: &'static [(&'static str, Value)],
    /// How many positionals are files.
    max_files: usize,
    /// Only a PowerShell verb (in POSIX shells, `type` is not a file reader).
    powershell: bool,
}

const ALL: usize = usize::MAX;

const VERBS: &[Verb] = &[
    Verb {
        names: &["cat", "less", "wc"],
        kind: TouchKind::Read,
        pattern_first: false,
        flags: &[],
        max_files: ALL,
        powershell: false,
    },
    Verb {
        names: &["head", "tail"],
        kind: TouchKind::Read,
        pattern_first: false,
        flags: &[
            ("-n", Value::Skip),
            ("-c", Value::Skip),
            ("--lines", Value::Skip),
            ("--bytes", Value::Skip),
        ],
        max_files: ALL,
        powershell: false,
    },
    Verb {
        names: &["grep", "egrep", "fgrep", "rg"],
        kind: TouchKind::Read,
        pattern_first: true,
        flags: &[
            ("-e", Value::Pattern),
            ("--regexp", Value::Pattern),
            ("-f", Value::PatternFile),
            ("--file", Value::PatternFile),
            ("-A", Value::Skip),
            ("-B", Value::Skip),
            ("-C", Value::Skip),
            ("-m", Value::Skip),
            ("-g", Value::Skip),
            ("-t", Value::Skip),
            ("-T", Value::Skip),
            ("-j", Value::Skip),
            ("-M", Value::Skip),
            ("-r", Value::Skip),
            ("-E", Value::Skip),
            ("--max-count", Value::Skip),
            ("--after-context", Value::Skip),
            ("--before-context", Value::Skip),
            ("--context", Value::Skip),
            ("--glob", Value::Skip),
            ("--iglob", Value::Skip),
            ("--type", Value::Skip),
            ("--type-not", Value::Skip),
            ("--include", Value::Skip),
            ("--exclude", Value::Skip),
            ("--replace", Value::Skip),
            ("--encoding", Value::Skip),
        ],
        max_files: ALL,
        powershell: false,
    },
    Verb {
        names: &["diff"],
        kind: TouchKind::Read,
        pattern_first: false,
        flags: &[
            ("-U", Value::Skip),
            ("-C", Value::Skip),
            ("-L", Value::Skip),
            ("-x", Value::Skip),
            ("-I", Value::Skip),
            ("--label", Value::Skip),
            ("--exclude", Value::Skip),
        ],
        max_files: ALL,
        powershell: false,
    },
    Verb {
        names: &["jq"],
        kind: TouchKind::Read,
        pattern_first: true,
        flags: &[
            ("-f", Value::PatternFile),
            ("--from-file", Value::PatternFile),
            ("--arg", Value::Skip),
            ("--argjson", Value::Skip),
            ("--indent", Value::Skip),
        ],
        max_files: ALL,
        powershell: false,
    },
    Verb {
        names: &["nl", "od"],
        kind: TouchKind::Read,
        pattern_first: false,
        flags: &[
            ("-w", Value::Skip),
            ("-s", Value::Skip),
            ("-v", Value::Skip),
            ("-i", Value::Skip),
            ("-b", Value::Skip),
            ("-n", Value::Skip),
            ("-A", Value::Skip),
            ("-j", Value::Skip),
            ("-N", Value::Skip),
            ("-t", Value::Skip),
        ],
        max_files: ALL,
        powershell: false,
    },
    Verb {
        names: &["sed"],
        kind: TouchKind::Read,
        pattern_first: true,
        flags: &[
            ("-e", Value::Pattern),
            ("--expression", Value::Pattern),
            ("-f", Value::PatternFile),
            ("--file", Value::PatternFile),
            ("-l", Value::Skip),
        ],
        max_files: ALL,
        powershell: false,
    },
    Verb {
        names: &["tee"],
        kind: TouchKind::Write,
        pattern_first: false,
        flags: &[],
        max_files: ALL,
        powershell: false,
    },
    Verb {
        names: &["truncate"],
        kind: TouchKind::Write,
        pattern_first: false,
        flags: &[
            ("-s", Value::Skip),
            ("--size", Value::Skip),
            ("-r", Value::PatternFile),
            ("--reference", Value::PatternFile),
        ],
        max_files: ALL,
        powershell: false,
    },
    Verb {
        names: &["patch"],
        kind: TouchKind::Write,
        pattern_first: false,
        flags: &[
            ("-i", Value::PatternFile),
            ("--input", Value::PatternFile),
            ("-o", Value::Output),
            ("--output", Value::Output),
            ("-p", Value::Skip),
            ("-d", Value::Skip),
            ("-F", Value::Skip),
            ("-B", Value::Skip),
            ("-D", Value::Skip),
            ("-V", Value::Skip),
            ("-z", Value::Skip),
            ("-r", Value::Skip),
            ("--directory", Value::Skip),
            ("--fuzz", Value::Skip),
            ("--prefix", Value::Skip),
            ("--suffix", Value::Skip),
            ("--reject-file", Value::Skip),
        ],
        max_files: ALL,
        powershell: false,
    },
    Verb {
        names: &["get-content", "gc", "type"],
        kind: TouchKind::Read,
        pattern_first: false,
        flags: &[
            ("-path", Value::Path),
            ("-literalpath", Value::Path),
            ("-lp", Value::Path),
            ("-totalcount", Value::Skip),
            ("-head", Value::Skip),
            ("-first", Value::Skip),
            ("-tail", Value::Skip),
            ("-last", Value::Skip),
            ("-encoding", Value::Skip),
            ("-delimiter", Value::Skip),
            ("-readcount", Value::Skip),
            ("-stream", Value::Skip),
        ],
        max_files: ALL,
        powershell: true,
    },
    Verb {
        names: &["select-string", "sls"],
        kind: TouchKind::Read,
        pattern_first: true,
        flags: &[
            ("-pattern", Value::Pattern),
            ("-path", Value::Path),
            ("-literalpath", Value::Path),
            ("-context", Value::Skip),
            ("-encoding", Value::Skip),
        ],
        max_files: ALL,
        powershell: true,
    },
    Verb {
        names: &["set-content", "sc", "add-content", "ac", "out-file"],
        kind: TouchKind::Write,
        pattern_first: false,
        flags: &[
            ("-path", Value::Path),
            ("-literalpath", Value::Path),
            ("-filepath", Value::Path),
            ("-value", Value::Skip),
            ("-inputobject", Value::Skip),
            ("-encoding", Value::Skip),
            ("-width", Value::Skip),
        ],
        max_files: 1,
        powershell: true,
    },
];

/// One simple command.
fn command(words: &[Word], dialect: Dialect, depth: usize, out: &mut Out) {
    let mut rest = words;
    while let Some(first) = rest.first() {
        if is_assignment(first)
            || (first.literal && PREFIXES.contains(&verb_name(&first.text).as_str()))
        {
            rest = &rest[1..];
            // The flags and counts of a prefix (`sudo -E`, `xargs -0`, `timeout 10s`,
            // `nice -n 5`), not other values.
            while rest.first().is_some_and(|w| is_flag(w) || is_count(w)) {
                rest = &rest[1..];
            }
        } else {
            break;
        }
    }
    let Some((verb, args)) = rest.split_first() else {
        return;
    };
    if !verb.literal {
        unknown_args(args, out);
        return;
    }
    if is_script(verb) {
        out.push(&verb.text, exec());
        unknown_args(args, out);
        return;
    }
    let name = verb_name(&verb.text);
    match name.as_str() {
        "cd" | "pushd" | "chdir" | "set-location" | "sl" | "push-location" => {
            let target = args.iter().find(|w| !is_flag(w) || w.text == "-");
            out.cd(target.filter(|w| w.text != "-"));
        }
        "popd" | "pop-location" => out.cd(None),
        "python" | "py" => python(args, out),
        "bash" | "sh" | "zsh" | "dash" => posix_shell(args, depth, out),
        "node" => node(args, out),
        "pwsh" | "powershell" => powershell(args, depth, out),
        "source" | "." => {
            if let Some(at) = args.iter().position(|w| !is_flag(w)) {
                out.script(&args[at]);
                unknown_args(&args[at + 1..], out);
            }
        }
        "uv" => uv(args, dialect, depth, out),
        "mise" => mise(args, dialect, depth, out),
        _ if SILENT.contains(&name.as_str()) => {}
        _ => match VERBS.iter().find(|v| {
            v.names.contains(&name.as_str()) && (!v.powershell || dialect == Dialect::PowerShell)
        }) {
            Some(spec) => verb_args(spec, args, dialect, out),
            None => unknown_args(args, out),
        },
    }
}

/// The arguments of an unknown verb: a file among them, or as a flag's `=value`, is unknown.
fn unknown_args(args: &[Word], out: &mut Out) {
    for word in args {
        if is_flag(word) {
            if let Some((_, value)) = word.text.split_once('=') {
                let value = Word {
                    text: value.to_owned(),
                    literal: word.literal,
                };
                out.file(&value, unknown());
            }
        } else {
            out.file(word, unknown());
        }
    }
}

/// Whether `sed`'s flag `word` asks for an in-place edit (`-i`, `-i.bak`, `-ni`,
/// `--in-place`).
fn in_place(word: &Word) -> bool {
    if let Some(long) = word.text.strip_prefix("--") {
        return long == "in-place" || long.starts_with("in-place=");
    }
    word.text
        .strip_prefix('-')
        .is_some_and(|cluster| cluster.split('.').next().is_some_and(|c| c.contains('i')))
}

fn verb_args(verb: &Verb, args: &[Word], dialect: Dialect, out: &mut Out) {
    let same = |a: &str, b: &str| match dialect {
        Dialect::Posix => a == b,
        Dialect::PowerShell => a.eq_ignore_ascii_case(b),
    };
    let mut kind = verb.kind;
    if verb.names.contains(&"sed") && args.iter().any(|w| is_flag(w) && in_place(w)) {
        kind = TouchKind::Write;
    }
    let mut pattern_given = !verb.pattern_first;
    let mut files: Vec<&Word> = Vec::new();
    let mut i = 0;
    let mut only_positionals = false;
    while i < args.len() {
        let word = &args[i];
        i += 1;
        if only_positionals || !is_flag(word) {
            if pattern_given {
                files.push(word);
            } else {
                pattern_given = true;
            }
            continue;
        }
        if word.text == "--" && dialect == Dialect::Posix {
            only_positionals = true;
            continue;
        }
        // `--name=value`, `-n5`, or `--name value`.
        let (name, inline) = match word.text.split_once('=') {
            Some((name, value)) if word.text.starts_with("--") => (name, Some(value.to_owned())),
            _ => (word.text.as_str(), None),
        };
        let (role, inline) = match verb.flags.iter().find(|(flag, _)| same(flag, name)) {
            Some((_, role)) => (Some(*role), inline),
            None if dialect == Dialect::Posix && !name.starts_with("--") => verb
                .flags
                .iter()
                .find(|(flag, _)| flag.len() == 2 && name.starts_with(flag) && name.len() > 2)
                .map_or((None, None), |(flag, role)| {
                    (Some(*role), Some(name[flag.len()..].to_owned()))
                }),
            None => (None, None),
        };
        let Some(role) = role else {
            continue;
        };
        let value = if let Some(text) = inline {
            Some(Word {
                text,
                literal: word.literal,
            })
        } else {
            i += 1;
            args.get(i - 1).cloned()
        };
        match role {
            Value::Skip => {}
            Value::Pattern => pattern_given = true,
            Value::PatternFile => {
                pattern_given = true;
                if let Some(value) = &value {
                    out.file(value, TouchKind::Read);
                }
            }
            Value::Path => {
                if let Some(value) = &value {
                    out.file(value, kind);
                }
            }
            Value::Output => {
                if let Some(value) = &value {
                    out.file(value, TouchKind::Write);
                }
            }
        }
    }
    for word in files.into_iter().take(verb.max_files) {
        out.file(word, kind);
    }
}

/// `python [flags] script args`, `python -m module args`, `python -c code args`.
fn python(args: &[Word], out: &mut Out) {
    let mut i = 0;
    while let Some(word) = args.get(i) {
        i += 1;
        match word.text.as_str() {
            "-c" | "-m" => {
                unknown_args(args.get(i + 1..).unwrap_or_default(), out);
                return;
            }
            "-W" | "-X" => i += 1,
            "--" => break,
            _ if is_flag(word) => {}
            _ => {
                out.script(word);
                unknown_args(&args[i..], out);
                return;
            }
        }
    }
    if let Some(script) = args.get(i) {
        out.script(script);
        unknown_args(&args[i + 1..], out);
    }
}

/// `bash [flags] script args`, `bash -c 'command'`.
fn posix_shell(args: &[Word], depth: usize, out: &mut Out) {
    let mut i = 0;
    while let Some(word) = args.get(i) {
        i += 1;
        match word.text.as_str() {
            "-c" => {
                if let Some(text) = args.get(i) {
                    classify(&text.text, Dialect::Posix, depth + 1, out);
                }
                return;
            }
            "-o" | "+o" | "-O" | "+O" => i += 1,
            "--" => break,
            _ if is_flag(word) || word.text.starts_with('+') => {}
            _ => {
                out.script(word);
                unknown_args(&args[i..], out);
                return;
            }
        }
    }
    if let Some(script) = args.get(i) {
        out.script(script);
        unknown_args(&args[i + 1..], out);
    }
}

/// `node [flags] script args`, `node -e code`.
fn node(args: &[Word], out: &mut Out) {
    let mut i = 0;
    while let Some(word) = args.get(i) {
        i += 1;
        match word.text.as_str() {
            "-e" | "--eval" | "-p" | "--print" => {
                unknown_args(args.get(i + 1..).unwrap_or_default(), out);
                return;
            }
            "-r" | "--require" | "--import" | "--loader" => i += 1,
            "--" => break,
            _ if is_flag(word) => {}
            _ => {
                out.script(word);
                unknown_args(&args[i..], out);
                return;
            }
        }
    }
    if let Some(script) = args.get(i) {
        out.script(script);
        unknown_args(&args[i + 1..], out);
    }
}

/// `pwsh -File script args`, `pwsh -Command text`, `pwsh script args`.
fn powershell(args: &[Word], depth: usize, out: &mut Out) {
    let mut i = 0;
    while let Some(word) = args.get(i) {
        i += 1;
        match word.text.to_lowercase().as_str() {
            "-file" | "-f" => {
                if let Some(script) = args.get(i) {
                    out.script(script);
                }
                unknown_args(args.get(i + 1..).unwrap_or_default(), out);
                return;
            }
            "-command" | "-c" => {
                let text: Vec<&str> = args[i..].iter().map(|w| w.text.as_str()).collect();
                classify(&text.join(" "), Dialect::PowerShell, depth + 1, out);
                return;
            }
            "-encodedcommand" | "-e" | "-ec" => return,
            "-executionpolicy" | "-ep" | "-workingdirectory" | "-wd" | "-outputformat" | "-of"
            | "-inputformat" | "-if" | "-windowstyle" | "-w" | "-configurationname"
            | "-settingsfile" => i += 1,
            _ if is_flag(word) => {}
            _ => {
                if is_script(word) {
                    out.script(word);
                    unknown_args(&args[i..], out);
                } else {
                    let text: Vec<&str> = args[i - 1..].iter().map(|w| w.text.as_str()).collect();
                    classify(&text.join(" "), Dialect::PowerShell, depth + 1, out);
                }
                return;
            }
        }
    }
}

/// `uv [flags] run [flags] command...`: the command is classified on its own.
fn uv(args: &[Word], dialect: Dialect, depth: usize, out: &mut Out) {
    const VALUES: &[&str] = &[
        "--with",
        "-w",
        "--python",
        "-p",
        "--project",
        "--directory",
        "--package",
        "--extra",
        "--group",
        "--env-file",
        "--with-requirements",
        "--with-editable",
        "--index",
        "--from",
    ];
    let Some(run) = args.iter().position(|w| !is_flag(w)) else {
        return;
    };
    if args[run].text != "run" {
        unknown_args(&args[run + 1..], out);
        return;
    }
    let mut i = run + 1;
    while let Some(word) = args.get(i) {
        if word.text == "--" {
            i += 1;
            break;
        }
        if !is_flag(word) {
            break;
        }
        i += if VALUES.contains(&word.text.as_str()) {
            2
        } else {
            1
        };
    }
    command(args.get(i..).unwrap_or_default(), dialect, depth, out);
}

/// `mise exec [tools] -- command...` (or `mise x`): the command is classified on its own.
fn mise(args: &[Word], dialect: Dialect, depth: usize, out: &mut Out) {
    let Some(sub) = args.iter().position(|w| !is_flag(w)) else {
        return;
    };
    if !matches!(args[sub].text.as_str(), "exec" | "x") {
        unknown_args(&args[sub + 1..], out);
        return;
    }
    if let Some(dash) = args[sub..].iter().position(|w| w.text == "--") {
        command(&args[sub + dash + 1..], dialect, depth, out);
    }
}

#[cfg(test)]
mod tests;

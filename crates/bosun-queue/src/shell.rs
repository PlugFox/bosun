//! Just enough of the POSIX shell grammar to lint a verdict: the programs it runs and the shape
//! of its last pipeline. Never used to run anything; the verdict shell does that.
//!
//! A function the script defines (`name() ...`, `function name ...`) is not a program: calls to
//! it after its definition are left out of [`Analysis::programs`]; a call before it is still a
//! program, as the shell runs top to bottom.
//!
//! Constructs this reader does not follow (`case`, here-documents) make the analysis
//! [`Analysis::opaque`]: the program list is then incomplete and lint checks only what it found
//! before them.

use std::collections::BTreeSet;

/// A word after quote removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Word {
    /// The text with quotes and escapes removed.
    pub(crate) text: String,
    /// No expansion or glob in it: the text is what the shell will see.
    pub(crate) literal: bool,
}

/// A redirection: `2>/dev/null`, `&>log`, `>&2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Redirect {
    /// The file descriptor written before the operator, if any.
    pub(crate) fd: Option<u32>,
    /// The operator.
    pub(crate) op: &'static str,
    /// Its target word.
    pub(crate) target: String,
}

/// One simple command of a pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Step {
    /// The command's words, program first, assignments and keywords left out.
    pub(crate) words: Vec<Word>,
    pub(crate) redirects: Vec<Redirect>,
}

impl Step {
    pub(crate) fn program(&self) -> Option<&str> {
        self.words.first().map(|w| w.text.as_str())
    }

    /// The program's file name without directories or a Windows extension.
    pub(crate) fn program_name(&self) -> Option<&str> {
        let program = self.program()?;
        let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
        Some(
            name.strip_suffix(".exe")
                .or_else(|| name.strip_suffix(".cmd"))
                .or_else(|| name.strip_suffix(".bat"))
                .unwrap_or(name),
        )
    }

    pub(crate) fn args(&self) -> impl Iterator<Item = &str> {
        self.words.iter().skip(1).map(|w| w.text.as_str())
    }
}

/// What [`analyse`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Analysis {
    /// Every literal word in command position, in order.
    pub(crate) programs: Vec<String>,
    /// The steps of the last pipeline.
    pub(crate) last_pipeline: Vec<Step>,
    /// The command uses syntax this reader does not follow; see the module docs.
    pub(crate) opaque: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(Word),
    Op(&'static str),
    Redirect(Option<u32>, &'static str),
}

/// Operators, longest first.
const OPERATORS: &[&str] = &[
    "&>>", "<<<", "&&", "||", "|&", ";;", "&>", "<<", ">>", ">|", ">&", "<&", "<>", "|", "&", ";",
    "(", ")", ">", "<",
];

fn is_redirect(op: &str) -> bool {
    op.contains(['<', '>'])
}

#[expect(
    clippy::too_many_lines,
    reason = "one character loop; splitting it scatters the quoting rules"
)]
fn lex(text: &str) -> (Vec<Token>, bool) {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut opaque = false;
    let mut word = String::new();
    let mut in_word = false;
    let mut literal = true;
    let mut i = 0;

    let finish =
        |tokens: &mut Vec<Token>, word: &mut String, in_word: &mut bool, literal: &mut bool| {
            if *in_word {
                tokens.push(Token::Word(Word {
                    text: std::mem::take(word),
                    literal: *literal,
                }));
            }
            *in_word = false;
            *literal = true;
        };

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => {
                finish(&mut tokens, &mut word, &mut in_word, &mut literal);
                i += 1;
            }
            '\n' => {
                finish(&mut tokens, &mut word, &mut in_word, &mut literal);
                tokens.push(Token::Op(";"));
                i += 1;
            }
            '#' if !in_word => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '\\' => {
                in_word = true;
                if chars.get(i + 1) == Some(&'\n') {
                    i += 2;
                } else {
                    if let Some(&next) = chars.get(i + 1) {
                        word.push(next);
                    }
                    i += 2;
                }
            }
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    match chars[i] {
                        '\\' if i + 1 < chars.len() => {
                            word.push(chars[i + 1]);
                            i += 2;
                        }
                        '$' | '`' => {
                            literal = false;
                            word.push(chars[i]);
                            i += 1;
                        }
                        other => {
                            word.push(other);
                            i += 1;
                        }
                    }
                }
                i += 1;
            }
            '$' | '`' => {
                in_word = true;
                literal = false;
                i = skip_expansion(&chars, i, &mut word);
            }
            '*' | '?' => {
                in_word = true;
                literal = false;
                word.push(c);
                i += 1;
            }
            _ => {
                let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
                if let Some(op) = OPERATORS.iter().find(|op| rest.starts_with(**op)) {
                    // Digits right before a redirection are its file descriptor.
                    let fd = (is_redirect(op) && in_word && literal && !word.is_empty())
                        .then(|| word.parse::<u32>().ok())
                        .flatten();
                    if fd.is_some() {
                        word.clear();
                        in_word = false;
                    } else {
                        finish(&mut tokens, &mut word, &mut in_word, &mut literal);
                    }
                    match *op {
                        "<<" | ";;" => opaque = true,
                        op if is_redirect(op) => tokens.push(Token::Redirect(fd, op)),
                        op => tokens.push(Token::Op(op)),
                    }
                    i += op.chars().count();
                } else {
                    in_word = true;
                    word.push(c);
                    i += 1;
                }
            }
        }
    }
    finish(&mut tokens, &mut word, &mut in_word, &mut literal);
    (tokens, opaque)
}

/// Skips `$name`, `${...}`, `$(...)`, `$((...))` or a backtick substitution starting at `i`,
/// copying it into `word`; returns the index after it.
fn skip_expansion(chars: &[char], mut i: usize, word: &mut String) -> usize {
    let start = i;
    if chars[i] == '`' {
        i += 1;
        while i < chars.len() && chars[i] != '`' {
            i += 1;
        }
        i = (i + 1).min(chars.len());
    } else {
        i += 1;
        if let Some(&open @ ('(' | '{')) = chars.get(i) {
            let close = if open == '(' { ')' } else { '}' };
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == open {
                    depth += 1;
                } else if chars[i] == close {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                i += 1;
            }
        } else {
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            // `$?`, `$1`, `$@`...
            if i == start + 1 && i < chars.len() {
                i += 1;
            }
        }
    }
    word.extend(&chars[start..i]);
    i
}

/// Words that open or close a compound command; the next word is again a command.
const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "!", "{", "}", "time",
];

fn is_assignment(word: &Word) -> bool {
    let Some((name, _)) = word.text.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn end_step(step: &mut Step, pipeline: &mut Vec<Step>) {
    if !step.words.is_empty() || !step.redirects.is_empty() {
        pipeline.push(std::mem::take(step));
    }
}
fn end_pipeline(step: &mut Step, pipeline: &mut Vec<Step>, pipelines: &mut Vec<Vec<Step>>) {
    end_step(step, pipeline);
    if !pipeline.is_empty() {
        pipelines.push(std::mem::take(pipeline));
    }
}

/// Records `name` as a function the script defines.
fn define(functions: &mut BTreeSet<String>, name: String) {
    if cfg!(mutant_queue_lint_functions_are_programs) {
        // Mutant (#7): a function the verdict defines is still resolved as a program.
        return;
    }
    functions.insert(name);
}

/// Reads `command` as the verdict shell would parse it, as far as this module follows.
pub(crate) fn analyse(command: &str) -> Analysis {
    let (tokens, mut opaque) = lex(command);
    let mut analysis = Analysis::default();
    let mut pipelines: Vec<Vec<Step>> = Vec::new();
    let mut pipeline: Vec<Step> = Vec::new();
    let mut step = Step::default();
    let mut command_position = true;
    // Inside `for x in ...` up to its `do`.
    let mut in_for = false;
    // Right after the `function` keyword: the next word is the name it defines.
    let mut function_keyword = false;
    // The functions defined so far: calls to them are not programs.
    let mut functions: BTreeSet<String> = BTreeSet::new();
    let mut tokens = tokens.into_iter().peekable();

    while let Some(token) = tokens.next() {
        match token {
            Token::Op("|" | "|&") => {
                end_step(&mut step, &mut pipeline);
                command_position = true;
            }
            Token::Op("(") if !step.words.is_empty() => {
                // `name() { ... }`: a function definition. The name is not a program, and from
                // here on calls to it are not either; its body follows as a command.
                let defines = step.words.len() == 1
                    && step.redirects.is_empty()
                    && matches!(tokens.peek(), Some(Token::Op(")")));
                if !defines {
                    opaque = true;
                    break;
                }
                tokens.next();
                if let Some(name) = step.words.pop() {
                    if name.literal && !functions.contains(&name.text) {
                        // It was recorded as a program when it was read.
                        analysis.programs.pop();
                    }
                    define(&mut functions, name.text);
                }
                command_position = true;
            }
            Token::Op(_) => {
                end_pipeline(&mut step, &mut pipeline, &mut pipelines);
                command_position = true;
            }
            Token::Redirect(fd, op) => {
                let target = match tokens.peek() {
                    Some(Token::Word(w)) => w.text.clone(),
                    _ => String::new(),
                };
                if !target.is_empty() {
                    tokens.next();
                }
                step.redirects.push(Redirect { fd, op, target });
            }
            Token::Word(word) => {
                if in_for {
                    if word.text == "do" {
                        in_for = false;
                        command_position = true;
                    }
                    continue;
                }
                if function_keyword {
                    // `function name { ... }` or `function name() { ... }`.
                    function_keyword = false;
                    if matches!(tokens.peek(), Some(Token::Op("("))) {
                        tokens.next();
                        if !matches!(tokens.next(), Some(Token::Op(")"))) {
                            opaque = true;
                            break;
                        }
                    }
                    define(&mut functions, word.text);
                    continue;
                }
                if !command_position {
                    step.words.push(word);
                    continue;
                }
                match word.text.as_str() {
                    "for" | "select" => in_for = true,
                    "function" => function_keyword = true,
                    "case" => {
                        opaque = true;
                        break;
                    }
                    keyword if KEYWORDS.contains(&keyword) => {}
                    _ if is_assignment(&word) => {}
                    _ => {
                        if word.literal && !functions.contains(&word.text) {
                            analysis.programs.push(word.text.clone());
                        }
                        step.words.push(word);
                        command_position = false;
                    }
                }
            }
        }
    }
    end_pipeline(&mut step, &mut pipeline, &mut pipelines);
    analysis.last_pipeline = pipelines.pop().unwrap_or_default();
    analysis.opaque = opaque;
    analysis
}

#[cfg(test)]
mod tests {
    use super::*;

    fn programs(command: &str) -> Vec<String> {
        analyse(command).programs
    }

    #[test]
    fn programs_in_lists_pipelines_and_compounds() {
        assert_eq!(
            programs("RUST_LOG=x cargo test -p a 2>&1 | grep -q ok && ./tool/check.sh; echo done"),
            ["cargo", "grep", "./tool/check.sh", "echo"]
        );
        assert_eq!(
            programs("if test -f x; then make; else (cd a && ninja); fi"),
            ["test", "make", "cd", "ninja"]
        );
        assert_eq!(programs("for f in a b; do wc -l $f; done"), ["wc"]);
        assert_eq!(programs("! grep -q bad log"), ["grep"]);
        assert_eq!(
            programs("'my tool' --x # a comment\nnext"),
            ["my tool", "next"]
        );
    }

    #[test]
    fn expansions_are_not_programs() {
        assert_eq!(
            programs("$CARGO test && \"$(which x)\" run"),
            Vec::<String>::new()
        );
        assert_eq!(programs("echo $(missing) `other`"), ["echo"]);
        assert_eq!(programs("./build/*.exe"), Vec::<String>::new());
    }

    #[test]
    fn unfollowed_syntax_is_opaque() {
        assert!(analyse("case $x in a) foo;; esac").opaque);
        assert!(analyse("cat <<EOF\nx\nEOF").opaque);
        assert!(!analyse("cargo test").opaque);
    }

    #[test]
    fn functions_the_script_defines_are_not_programs() {
        let analysis = analyse("f; f() { g | wc -l; }; f && f; h");
        assert!(!analysis.opaque);
        assert_eq!(analysis.programs, ["f", "g", "wc", "h"]);
        assert_eq!(
            programs("function a { b; }; function c() ( d ); a; c; e"),
            ["b", "d", "e"]
        );
        assert_eq!(programs("f(){ true; };f"), ["true"]);
    }

    #[test]
    fn redirects_and_the_last_pipeline() {
        let analysis = analyse("make; cargo test 2>&1 | tail -n 1 >/dev/null");
        let steps = &analysis.last_pipeline;
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].program(), Some("cargo"));
        assert_eq!(
            steps[0].redirects,
            [Redirect {
                fd: Some(2),
                op: ">&",
                target: "1".into()
            }]
        );
        assert_eq!(steps[1].args().collect::<Vec<_>>(), ["-n", "1"]);
        assert_eq!(
            steps[1].redirects,
            [Redirect {
                fd: None,
                op: ">",
                target: "/dev/null".into()
            }]
        );
        let step = &analyse("C:/bin/Tool.exe x").last_pipeline[0];
        assert_eq!(step.program_name(), Some("Tool"));
    }
}

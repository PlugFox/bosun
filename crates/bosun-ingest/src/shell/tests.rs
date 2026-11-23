//! Synthetic commands for each rule of #44 A.

use super::*;

use TouchKind::{Exec, Read, Unknown, Write};

fn posix(command: &str) -> Vec<(String, TouchKind)> {
    touches(command, Dialect::Posix)
}

fn ps(command: &str) -> Vec<(String, TouchKind)> {
    touches(command, Dialect::PowerShell)
}

fn pairs(list: &[(&str, TouchKind)]) -> Vec<(String, TouchKind)> {
    list.iter().map(|(p, k)| ((*p).to_owned(), *k)).collect()
}

#[track_caller]
fn check(dialect: Dialect, command: &str, want: &[(&str, TouchKind)]) {
    assert_eq!(touches(command, dialect), pairs(want), "{command}");
}

#[test]
fn read_verbs_read_their_files() {
    let cases: &[(&str, &[(&str, TouchKind)])] = &[
        ("cat src/lib.rs", &[("src/lib.rs", Read)]),
        ("cat a.rs b.rs", &[("a.rs", Read), ("b.rs", Read)]),
        ("head -n 40 src/lib.rs", &[("src/lib.rs", Read)]),
        ("head -n40 src/lib.rs", &[("src/lib.rs", Read)]),
        ("tail -20 log/run.txt", &[("log/run.txt", Read)]),
        ("grep -n synthetic_fn src/lib.rs", &[("src/lib.rs", Read)]),
        // The pattern looks like a file and is not one.
        ("grep -rn lib.rs src/main.rs", &[("src/main.rs", Read)]),
        (
            "grep -e lib.rs -e x.rs src/main.rs",
            &[("src/main.rs", Read)],
        ),
        (
            "grep -f pats.txt src/main.rs",
            &[("pats.txt", Read), ("src/main.rs", Read)],
        ),
        ("grep -A 3 pattern src/main.rs", &[("src/main.rs", Read)]),
        ("rg --type rust synthetic src/a.rs", &[("src/a.rs", Read)]),
        ("rg --glob=*.rs synthetic src/a.rs", &[("src/a.rs", Read)]),
        (
            "wc -l src/a.rs src/b.rs",
            &[("src/a.rs", Read), ("src/b.rs", Read)],
        ),
        (
            "diff -u old.txt new.txt",
            &[("old.txt", Read), ("new.txt", Read)],
        ),
        ("jq .name package.json", &[("package.json", Read)]),
        (
            "jq -f filter.jq data.json",
            &[("filter.jq", Read), ("data.json", Read)],
        ),
        ("nl -b a src/a.rs", &[("src/a.rs", Read)]),
        ("od -A x -t x1z data.bin", &[("data.bin", Read)]),
        ("less README.md", &[("README.md", Read)]),
        ("sed -n '1,40p' src/lib.rs", &[("src/lib.rs", Read)]),
        ("sed -n -e 's/a/b/p' src/lib.rs", &[("src/lib.rs", Read)]),
        (
            "sed -f edit.sed src/lib.rs",
            &[("edit.sed", Read), ("src/lib.rs", Read)],
        ),
        ("wc -l < src/lib.rs", &[("src/lib.rs", Read)]),
    ];
    for (command, want) in cases {
        check(Dialect::Posix, command, want);
    }
}

#[test]
fn write_verbs_and_output_redirects_write() {
    let cases: &[(&str, &[(&str, TouchKind)])] = &[
        ("sed -i 's/a/b/' src/lib.rs", &[("src/lib.rs", Write)]),
        (
            "sed -i.bak -e 's/a/b/' src/lib.rs",
            &[("src/lib.rs", Write)],
        ),
        ("sed -ni 's/a/b/p' src/lib.rs", &[("src/lib.rs", Write)]),
        (
            "sed --in-place 's/a/b/' src/lib.rs",
            &[("src/lib.rs", Write)],
        ),
        ("tee -a out.log", &[("out.log", Write)]),
        ("truncate -s 0 out.log", &[("out.log", Write)]),
        ("patch -p1 -i fix.diff", &[("fix.diff", Read)]),
        (
            "patch src/lib.rs fix.diff",
            &[("src/lib.rs", Write), ("fix.diff", Write)],
        ),
        ("echo done > out.txt", &[("out.txt", Write)]),
        ("echo done >> out.txt", &[("out.txt", Write)]),
        ("cargo test 2> err.txt", &[("err.txt", Write)]),
        ("cargo test &> all.txt", &[("all.txt", Write)]),
        ("cargo test >all.txt 2>&1", &[("all.txt", Write)]),
        ("cargo test > /dev/null 2>&1", &[]),
        ("cat a.rs > b.rs", &[("b.rs", Write), ("a.rs", Read)]),
    ];
    for (command, want) in cases {
        check(Dialect::Posix, command, want);
    }
}

#[test]
fn an_interpreter_runs_its_script() {
    let cases: &[(&str, &[(&str, TouchKind)])] = &[
        ("python tool/x.py", &[("tool/x.py", Exec)]),
        ("python3 -u tool/x.py --fast", &[("tool/x.py", Exec)]),
        ("python3.12 tool/x.py", &[("tool/x.py", Exec)]),
        ("py -3 tool/x.py", &[("tool/x.py", Exec)]),
        ("python -W ignore tool/x.py", &[("tool/x.py", Exec)]),
        (
            "python -m pytest tests/test_a.py",
            &[("tests/test_a.py", Unknown)],
        ),
        ("python -c 'print(1)'", &[]),
        ("bash tool/run", &[("tool/run", Exec)]),
        ("sh -e tool/run.sh arg", &[("tool/run.sh", Exec)]),
        ("bash -c 'cat src/lib.rs'", &[("src/lib.rs", Read)]),
        ("node tool/x.js", &[("tool/x.js", Exec)]),
        ("node -e 'console.log(1)'", &[]),
        ("source tool/env.sh", &[("tool/env.sh", Exec)]),
        (". tool/env.sh", &[("tool/env.sh", Exec)]),
        ("uv run tool/x.py", &[("tool/x.py", Exec)]),
        (
            "uv run --with rich python tool/x.py",
            &[("tool/x.py", Exec)],
        ),
        (
            "uv run pytest tests/test_a.py",
            &[("tests/test_a.py", Unknown)],
        ),
        ("mise exec -- python tool/x.py", &[("tool/x.py", Exec)]),
        (
            "mise exec python@3.12 -- python tool/x.py",
            &[("tool/x.py", Exec)],
        ),
        ("./tool/x.sh --fast", &[("./tool/x.sh", Exec)]),
        ("tool/x.py", &[("tool/x.py", Exec)]),
        // The script's own arguments are named by an unknown program.
        (
            "python tool/x.py src/lib.rs",
            &[("tool/x.py", Exec), ("src/lib.rs", Unknown)],
        ),
        ("python tool/x.py -- cargo test", &[("tool/x.py", Exec)]),
    ];
    for (command, want) in cases {
        check(Dialect::Posix, command, want);
    }
}

/// Mutant `mutant_ingest_unknown_is_read`.
#[test]
fn an_unknown_verb_names_an_unknown_touch() {
    let cases: &[(&str, &[(&str, TouchKind)])] = &[
        ("rustfmt --check src/lib.rs", &[("src/lib.rs", Unknown)]),
        ("git add src/lib.rs", &[("src/lib.rs", Unknown)]),
        (
            "cargo test --manifest-path=crates/a/Cargo.toml",
            &[("crates/a/Cargo.toml", Unknown)],
        ),
        ("cp a.rs b.rs", &[("a.rs", Unknown), ("b.rs", Unknown)]),
        ("$TOOL src/lib.rs", &[("src/lib.rs", Unknown)]),
    ];
    for (command, want) in cases {
        check(Dialect::Posix, command, want);
    }
}

/// #44 A: the other mention keeps its own kind.
#[test]
fn a_script_read_and_run_is_one_read_and_one_exec() {
    check(
        Dialect::Posix,
        "cat x.py && python x.py",
        &[("x.py", Read), ("x.py", Exec)],
    );
    // The predecessor's trap: a `sed -n` in the same command does not make the run a read.
    check(
        Dialect::Posix,
        "sed -n '1,40p' src/lib.rs && python tool/check.py -- cargo test",
        &[("src/lib.rs", Read), ("tool/check.py", Exec)],
    );
}

#[test]
fn every_segment_is_classified_on_its_own() {
    let cases: &[(&str, &[(&str, TouchKind)])] = &[
        ("cat a.rs | grep x", &[("a.rs", Read)]),
        ("cat a.rs | tee b.rs", &[("a.rs", Read), ("b.rs", Write)]),
        ("cat a.rs; python b.py", &[("a.rs", Read), ("b.py", Exec)]),
        ("cat a.rs || python b.py", &[("a.rs", Read), ("b.py", Exec)]),
        ("python b.py & cat a.rs", &[("b.py", Exec), ("a.rs", Read)]),
        ("cat a.rs\npython b.py", &[("a.rs", Read), ("b.py", Exec)]),
        (
            "(cat a.rs) && python b.py",
            &[("a.rs", Read), ("b.py", Exec)],
        ),
        (
            "echo $(cat a.rs) && python b.py",
            &[("a.rs", Read), ("b.py", Exec)],
        ),
        ("echo \"$(cat a.rs)\"", &[("a.rs", Read)]),
        ("echo `cat a.rs`", &[("a.rs", Read)]),
        ("diff <(cat a.rs) b.rs", &[("a.rs", Read), ("b.rs", Read)]),
        ("cargo test 2>&1 | tail -5", &[]),
        ("cat a.rs |& grep x", &[("a.rs", Read)]),
        ("if true; then cat a.rs; fi", &[("a.rs", Read)]),
        (
            "for f in a.rs b.rs; do cat $f; done",
            &[("a.rs", Unknown), ("b.rs", Unknown)],
        ),
        ("RUST_LOG=debug sudo -E cat a.rs", &[("a.rs", Read)]),
        ("time python b.py", &[("b.py", Exec)]),
        ("cat a.rs # and b.rs", &[("a.rs", Read)]),
        ("cat a.rs \\\n  b.rs", &[("a.rs", Read), ("b.rs", Read)]),
    ];
    for (command, want) in cases {
        check(Dialect::Posix, command, want);
    }
}

#[test]
fn a_here_document_body_is_not_a_command() {
    check(
        Dialect::Posix,
        "cat > notes.md <<'EOF'\npython secret.py\ncat src/lib.rs\nEOF\ncat a.rs",
        &[("notes.md", Write), ("a.rs", Read)],
    );
    check(
        Dialect::Posix,
        "cat <<-EOF > notes.md\n\tcat src/lib.rs\n\tEOF\npython b.py",
        &[("notes.md", Write), ("b.py", Exec)],
    );
    check(Dialect::Posix, "grep x <<< 'a.rs b.rs'", &[]);
}

#[test]
fn a_cd_moves_the_relative_paths_after_it() {
    let cases: &[(&str, &[(&str, TouchKind)])] = &[
        (
            "cd crates/a && cat src/lib.rs",
            &[("crates/a/src/lib.rs", Read)],
        ),
        (
            "cd crates && cd a && python x.py",
            &[("crates/a/x.py", Exec)],
        ),
        (
            "cd /fixture/repo-b && cat x.rs",
            &[("/fixture/repo-b/x.rs", Read)],
        ),
        ("cat a.rs && cd $DIR && cat b.rs", &[("a.rs", Read)]),
        ("cd - && cat b.rs", &[]),
        (
            "cd $DIR && cat /fixture/repo-a/b.rs",
            &[("/fixture/repo-a/b.rs", Read)],
        ),
    ];
    for (command, want) in cases {
        check(Dialect::Posix, command, want);
    }
}

/// #2 and #76: an argument that is not a file never leaves the module.
#[test]
fn arguments_that_are_not_files_are_dropped() {
    let secret = "s3cr3t-value";
    let commands = [
        format!("git commit -m \"fix lib.rs {secret}\""),
        format!("curl -H 'Authorization: Bearer {secret}' https://example.invalid/x.json"),
        format!("API_TOKEN={secret} gh pr list"),
        format!("echo {secret}.json src/lib.rs"),
        format!("grep {secret}.rs"),
        format!("export TOKEN={secret}.txt"),
        "git checkout origin/main".to_owned(),
        "cargo bench -- 1.2.3".to_owned(),
        "cat $FILE *.rs ~/notes.md {a,b}.rs".to_owned(),
        "ls -la src/".to_owned(),
        "printf '%s' a.rs".to_owned(),
        "cat user@example.invalid:x.rs".to_owned(),
    ];
    for command in &commands {
        let got = posix(command);
        assert!(
            got.iter().all(|(path, _)| !path.contains(secret)),
            "{command}: {got:?}"
        );
        assert!(got.is_empty(), "{command}: {got:?}");
    }
}

#[test]
fn windows_paths_are_kept_whole() {
    check(
        Dialect::Posix,
        r"cat C:\fixture\repo-a\src\lib.rs",
        &[(r"C:\fixture\repo-a\src\lib.rs", Read)],
    );
    check(
        Dialect::Posix,
        "python C:/fixture/repo-a/tool/x.py",
        &[("C:/fixture/repo-a/tool/x.py", Exec)],
    );
    check(
        Dialect::Posix,
        r#"cat "C:\fixture\repo-a\a.rs""#,
        &[(r"C:\fixture\repo-a\a.rs", Read)],
    );
}

#[test]
fn powershell_commands_are_classified() {
    let cases: &[(&str, &[(&str, TouchKind)])] = &[
        (
            r"Get-Content src\lib.rs -TotalCount 20",
            &[(r"src\lib.rs", Read)],
        ),
        (r"gc -Path src\lib.rs", &[(r"src\lib.rs", Read)]),
        (r"type src\lib.rs", &[(r"src\lib.rs", Read)]),
        (
            "Select-String -Pattern lib.rs -Path src/main.rs",
            &[("src/main.rs", Read)],
        ),
        ("sls synthetic src/main.rs", &[("src/main.rs", Read)]),
        (
            "Set-Content -Path out.txt -Value a.rs",
            &[("out.txt", Write)],
        ),
        ("Set-Content out.txt a.rs", &[("out.txt", Write)]),
        (
            "cargo test | Out-File -FilePath log.txt",
            &[("log.txt", Write)],
        ),
        ("cargo test > log.txt", &[("log.txt", Write)]),
        ("cargo test > $null", &[]),
        (r"& .\tool\check.ps1 -Fast", &[(r".\tool\check.ps1", Exec)]),
        (r#"& "tool\check.ps1""#, &[(r"tool\check.ps1", Exec)]),
        (
            r"pwsh -NoProfile -File tool\x.ps1",
            &[(r"tool\x.ps1", Exec)],
        ),
        ("pwsh -Command Get-Content a.rs", &[("a.rs", Read)]),
        (
            r"python tool\x.py; Get-Content a.rs",
            &[(r"tool\x.py", Exec), ("a.rs", Read)],
        ),
        (
            "Get-ChildItem *.rs | ForEach-Object { Get-Content b.rs }",
            &[("b.rs", Read)],
        ),
        ("Get-Content $env:FILE", &[]),
        ("Write-Host \"a`\"b.rs\"", &[]),
        (
            "Set-Location crates; Get-Content a.rs",
            &[("crates/a.rs", Read)],
        ),
        (
            "$t = @'\nGet-Content secret.rs\n'@\nGet-Content a.rs",
            &[("a.rs", Read)],
        ),
        ("GET-CONTENT a.rs", &[("a.rs", Read)]),
        (
            "Copy-Item a.rs b.rs",
            &[("a.rs", Unknown), ("b.rs", Unknown)],
        ),
    ];
    for (command, want) in cases {
        check(Dialect::PowerShell, command, want);
    }
    // `type` reads only in PowerShell.
    assert_eq!(posix("type a.rs"), pairs(&[("a.rs", Unknown)]));
    assert_eq!(ps("type a.rs"), pairs(&[("a.rs", Read)]));
}

#[test]
fn deep_or_torn_commands_do_not_panic() {
    let deep = format!("{}cat a.rs{}", "$(".repeat(64), ")".repeat(64));
    assert_eq!(posix(&deep), Vec::new());
    for torn in [
        "cat 'a.rs",
        "cat \"a.rs",
        "echo $(cat a.rs",
        "echo `cat a.rs",
        "cat >",
        "cat <<EOF\nx",
        "\\",
        "`",
        "$",
        "${",
        "@'",
        "&",
        "|",
        "2>",
        "<(",
        "sed -e",
        "grep -A",
    ] {
        let _ = posix(torn);
        let _ = ps(torn);
    }
}

#[test]
fn what_looks_like_a_file() {
    for yes in [
        "a.rs",
        "src/lib.rs",
        r"src\lib.rs",
        "Cargo.toml",
        ".gitignore",
        "0001_stage_0.sql",
        "/fixture/repo-a/x.md",
        r"C:\fixture\x.json",
        "C:/fixture/x.json",
        "../x.rs",
    ] {
        assert!(path_like(yes), "{yes}");
    }
    for no in [
        "",
        "-x.rs",
        "src/",
        "Makefile",
        "1.2.3",
        "v1.2",
        "a b.rs",
        "$x.rs",
        "*.rs",
        "x.rs:1",
        "https://example.invalid/x.rs",
        "a@b.rs",
        "..",
        ".",
        "~/x.rs",
        "k=v.rs",
        "x.toolongextension1",
    ] {
        assert!(!path_like(no), "{no}");
    }
}

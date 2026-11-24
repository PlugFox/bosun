//! `fake-agent`, a scripted stand-in for a coding-agent CLI in tests (#7, #52): it replays a
//! synthetic stream fixture and performs git actions, so the daemon's spawn, classification
//! and verdict paths can be tested without a provider.
//!
//! # Invocation
//!
//! The script is named by `--fake-script <path>` anywhere in argv, or else by the variable
//! `FAKE_AGENT_SCRIPT`. With `--fake-script-dir <dir>` instead, the script is
//! `<dir>/<item>.script`, where `<item>` is the variable `BOSUN_ITEM` (the item the daemon
//! spawned the session for) with every character but ASCII letters, digits, `-` and `_` turned
//! into `_`: one fake agent can play a different part for each item of a run. Every other
//! argument is ignored, so a runner's real argv (the `claude` flags) can be passed unchanged.
//! The working directory is the one the spawner gives.
//!
//! With `--mcp-config <file>`, as the agent CLI does, every MCP server the file lists is
//! started and initialized before the script runs; one that fails is reported on standard
//! error and left out, and the script runs anyway. The servers are stopped (their stdin closed)
//! when the script ends.
//!
//! # Script
//!
//! One command per line; blank lines and lines starting with `#` are skipped. The first word is
//! the command, the rest of the line its argument. Relative paths of `replay` are resolved
//! against the script's directory, all other paths against the working directory.
//!
//! | Command | Effect |
//! | --- | --- |
//! | `replay <file>` | copies the file to standard output (a stream fixture) |
//! | `print <text>` | writes the text and a line break to standard output |
//! | `eprint <text>` | the same to standard error |
//! | `write <path> <text>` | writes the text and a line break to the file, creating its directories (an untracked file) |
//! | `commit <path> <message>` | writes the message to the file, then `git add` and `git commit -m <message>` |
//! | `git <args>` | runs git with the whitespace-separated arguments |
//! | `unset <name>` | the later `commit` and `git` commands run without the variable (an agent turning the trailer marker off, say) |
//! | `stdin <path>` | copies standard input to the file (to check the prompt) |
//! | `argv <path>` | writes the arguments, one per line (to check the command) |
//! | `sleep <ms>` | sleeps |
//! | `wait <path>` | waits until the file exists (at most 60 s, then a script error) |
//! | `mcp-tools <server> <path>` | writes the names the server's `tools/list` returned, one per line |
//! | `mcp <server> <tool> <json>` | calls the tool with the JSON object as its arguments; the result goes to standard error |
//! | `exit <code>` | exits with the code; the end of the script exits with 0 |
//! | `once <marker> <command>` | runs the command (`exit` too) only when the file `marker` does not exist, creating it first: a resumed or retried session of the same script plays a later part |
//!
//! A script error (a missing script, an unknown command, a failed git command) is reported on
//! standard error and exits with [`SCRIPT_ERROR`].

mod mcp;

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// The exit code of a script error, chosen to be unlike an agent CLI's.
const SCRIPT_ERROR: u8 = 125;

/// The variables `unset` removed from the git commands that follow.
static UNSET: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("fake-agent: {message}");
            ExitCode::from(SCRIPT_ERROR)
        }
    }
}

fn script_path(args: &[String]) -> Result<PathBuf, String> {
    if let Some(at) = args.iter().position(|a| a == "--fake-script-dir") {
        let dir = args
            .get(at + 1)
            .ok_or_else(|| "--fake-script-dir needs a directory".to_owned())?;
        let item = std::env::var("BOSUN_ITEM")
            .map_err(|_| "--fake-script-dir needs BOSUN_ITEM".to_owned())?;
        let name: String = item
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        return Ok(Path::new(dir).join(format!("{name}.script")));
    }
    if let Some(at) = args.iter().position(|a| a == "--fake-script") {
        return args
            .get(at + 1)
            .map(PathBuf::from)
            .ok_or_else(|| "--fake-script needs a path".to_owned());
    }
    std::env::var_os("FAKE_AGENT_SCRIPT")
        .map(PathBuf::from)
        .ok_or_else(|| "no script: pass --fake-script <path> or set FAKE_AGENT_SCRIPT".to_owned())
}

fn run() -> Result<u8, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let script = script_path(&args)?;
    let text = std::fs::read_to_string(&script)
        .map_err(|e| format!("cannot read the script {}: {e}", script.display()))?;
    let base = script.parent().unwrap_or(Path::new("."));
    let mut servers = match args.iter().position(|a| a == "--mcp-config") {
        Some(at) => {
            let file = args
                .get(at + 1)
                .ok_or_else(|| "--mcp-config needs a file".to_owned())?;
            mcp::Servers::start(Path::new(file))?
        }
        None => mcp::Servers::default(),
    };

    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |e: String| format!("{}:{}: {e}", script.display(), number + 1);
        let (command, rest) = line.split_once(' ').unwrap_or((line, ""));
        let (command, rest) = if command == "once" {
            let (marker, inner) = rest
                .trim()
                .split_once(' ')
                .ok_or_else(|| at("once needs a marker and a command".to_owned()))?;
            if Path::new(marker).exists() {
                continue;
            }
            write_file(Path::new(marker), "").map_err(at)?;
            inner.trim().split_once(' ').unwrap_or((inner.trim(), ""))
        } else {
            (command, rest)
        };
        let rest = rest.trim();
        if command == "exit" {
            return rest
                .parse()
                .map_err(|_| at(format!("bad exit code {rest:?}")));
        }
        step(command, rest, base, &args, &mut servers).map_err(at)?;
    }
    Ok(0)
}

fn step(
    command: &str,
    rest: &str,
    base: &Path,
    args: &[String],
    servers: &mut mcp::Servers,
) -> Result<(), String> {
    match command {
        "replay" => {
            let bytes = std::fs::read(base.join(rest)).map_err(io(rest))?;
            let mut out = std::io::stdout().lock();
            out.write_all(&bytes).map_err(io("stdout"))?;
            out.flush().map_err(io("stdout"))
        }
        "print" => {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{rest}").map_err(io("stdout"))?;
            out.flush().map_err(io("stdout"))
        }
        "eprint" => {
            eprintln!("{rest}");
            Ok(())
        }
        "write" => {
            let (path, text) = rest.split_once(' ').unwrap_or((rest, ""));
            write_file(Path::new(path), text)
        }
        "commit" => {
            let (path, message) = rest
                .split_once(' ')
                .ok_or_else(|| "commit needs a path and a message".to_owned())?;
            write_file(Path::new(path), message)?;
            git(&["add", "--", path])?;
            git(&["commit", "-q", "-m", message])
        }
        "git" => git(&rest.split_whitespace().collect::<Vec<_>>()),
        "unset" => {
            UNSET
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(rest.to_owned());
            Ok(())
        }
        "stdin" => {
            let mut input = Vec::new();
            std::io::stdin()
                .read_to_end(&mut input)
                .map_err(io("stdin"))?;
            std::fs::write(rest, input).map_err(io(rest))
        }
        "argv" => {
            let text: String = args.iter().flat_map(|a| [a.as_str(), "\n"]).collect();
            std::fs::write(rest, text).map_err(io(rest))
        }
        "sleep" => {
            let ms: u64 = rest.parse().map_err(|_| format!("bad sleep {rest:?}"))?;
            std::thread::sleep(Duration::from_millis(ms));
            Ok(())
        }
        "wait" => {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while !Path::new(rest).exists() {
                if std::time::Instant::now() > deadline {
                    return Err(format!("{rest} did not appear within 60 s"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        }
        "mcp-tools" => {
            let (server, path) = rest
                .split_once(' ')
                .ok_or_else(|| "mcp-tools needs a server and a path".to_owned())?;
            let text: String = servers
                .get(server)?
                .tools
                .iter()
                .flat_map(|t| {
                    [
                        t.as_str(),
                        "
",
                    ]
                })
                .collect();
            std::fs::write(path, text).map_err(io(path))
        }
        "mcp" => {
            let mut words = rest.splitn(3, ' ');
            let (Some(server), Some(tool)) = (words.next(), words.next()) else {
                return Err("mcp needs a server, a tool and arguments".to_owned());
            };
            let arguments: serde_json::Value = serde_json::from_str(words.next().unwrap_or("{}"))
                .map_err(|e| format!("mcp arguments: {e}"))?;
            let result = servers.call(server, tool, &arguments)?;
            eprintln!("{result}");
            Ok(())
        }
        other => Err(format!("unknown command {other:?}")),
    }
}

/// An I/O error with what it was about.
fn io(what: &str) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, format!("{text}\n")).map_err(|e| format!("{}: {e}", path.display()))
}

fn git(args: &[&str]) -> Result<(), String> {
    let mut command = Command::new("git");
    for name in UNSET.lock().unwrap_or_else(PoisonError::into_inner).iter() {
        command.env_remove(name);
    }
    let status = command
        .args(args)
        .status()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("git {} failed: {status}", args.join(" ")))
    }
}

//! `bosun install` and `bosun uninstall` (#6, #28): register and remove the logon process that
//! starts `bosund` for the current user.
//!
//! | Platform | Registration | Loaded with |
//! | --- | --- | --- |
//! | Windows | the Task Scheduler task `<name>`, at the user's logon, through `conhost.exe --headless` so no console window opens | `schtasks /Create /XML` |
//! | macOS | `~/Library/LaunchAgents/io.github.plugfox.<name>.plist`, run at load; output to `~/Library/Logs/bosun/<name>.log` | `launchctl bootstrap gui/<uid>` |
//! | Linux | `$XDG_CONFIG_HOME/systemd/user/<name>.service` (else `~/.config/...`), wanted by `default.target` | `systemctl --user enable` |
//!
//! The name is `bosun` unless `--name` says otherwise (a second daemon, or a test). Install is
//! idempotent: it rewrites the registration, stops a daemon it started before and starts the
//! daemon again from the registration, so a re-install after an upgrade runs the new binary.
//! Uninstall stops the daemon and removes the registration (and on macOS its log); with
//! nothing registered it does nothing.
//!
//! On Windows `schtasks /End` ends the hidden `conhost.exe` but leaves the `bosund` it hosts
//! running, so `install` and `uninstall` also end the daemon that answers `hello` at the
//! endpoint, by its pid (`taskkill /F`; stage 0 has no shutdown method in the API).
//!
//! No platform restarts the daemon when it exits: it exits on a bad configuration or when
//! another instance runs, and a restart would loop. `bosun doctor` reports a daemon that is
//! down. On Linux the user's service manager, and the daemon with it, stops at the last logout
//! unless lingering is enabled (`loginctl enable-linger`); that is the logon semantics of the
//! other platforms too.
//!
//! Every platform's code is compiled on every host and chosen at run time, so each is linted
//! everywhere; only the artefact generators (`artefact.rs`) are pure.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use crate::artefact::{self, Launch};

/// The platforms that have a logon registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Windows,
    Macos,
    Linux,
}

impl Platform {
    pub(crate) fn host() -> Result<Self, String> {
        if cfg!(windows) {
            Ok(Self::Windows)
        } else if cfg!(target_os = "macos") {
            Ok(Self::Macos)
        } else if cfg!(target_os = "linux") {
            Ok(Self::Linux)
        } else {
            Err(format!(
                "no logon registration on {}: start bosund by other means",
                std::env::consts::OS
            ))
        }
    }
}

/// The default registration name.
pub(crate) const DEFAULT_NAME: &str = "bosun";

/// Accepts a registration name: ASCII letters, digits, `-`, `_` and `.`, which every platform
/// takes as a task name, a label part and a unit name.
pub(crate) fn parse_name(text: &str) -> Result<String, String> {
    let ok = !text.is_empty()
        && text.len() <= 64
        && !text.starts_with('.')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(text.to_owned())
    } else {
        Err("a name is 1 to 64 ASCII letters, digits, `-`, `_` or `.`".to_owned())
    }
}

/// What the platform knows of a registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registration {
    /// Where it is: the task, or the file.
    pub(crate) place: String,
    /// The service manager has it loaded (launchd) or enabled (systemd); always true for a
    /// task that exists.
    pub(crate) loaded: bool,
    /// The daemon it runs, when it can be read back.
    pub(crate) daemon: Option<PathBuf>,
    /// `PATH` the registration gives the daemon, when it sets one.
    pub(crate) path_var: Option<String>,
}

/// Runs a command; its output, or why it could not run.
fn run(program: &str, args: &[&OsStr]) -> Result<Output, String> {
    Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))
}

/// Runs a command that must succeed.
fn must(program: &str, args: &[&OsStr]) -> Result<Output, String> {
    let output = run(program, args)?;
    if output.status.success() {
        return Ok(output);
    }
    let words: Vec<String> = args.iter().map(|a| a.to_string_lossy().into()).collect();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let said = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    Err(format!(
        "`{program} {}` failed ({}): {said}",
        words.join(" "),
        output.status
    ))
}

fn os(text: &str) -> &OsStr {
    OsStr::new(text)
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| "HOME is not set to an absolute path".to_owned())
}

/// Writes `bytes` to `path`, creating its directory.
fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn remove(path: &Path) -> Result<bool, String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
    }
}

/// `DOMAIN\user` of the current Windows user.
fn windows_user() -> Result<String, String> {
    let user = std::env::var("USERNAME").map_err(|_| "USERNAME is not set".to_owned())?;
    Ok(match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!(r"{domain}\{user}"),
        _ => user,
    })
}

/// The user's numeric id on macOS, from `id -u` (no `unsafe` for `getuid`).
fn macos_uid() -> Result<String, String> {
    let output = must("id", &[os("-u")])?;
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("`id -u` printed {uid:?}"));
    }
    Ok(uid)
}

/// `gui/<uid>`: the launchd domain of the user's login session.
fn macos_domain() -> Result<String, String> {
    Ok(format!("gui/{}", macos_uid()?))
}

fn macos_service(name: &str) -> Result<String, String> {
    Ok(format!(
        "{}/{}",
        macos_domain()?,
        artefact::macos_label(name)
    ))
}

fn macos_file(name: &str) -> Result<PathBuf, String> {
    Ok(artefact::macos_plist_file(
        &home()?,
        &artefact::macos_label(name),
    ))
}

fn macos_log(name: &str) -> Result<PathBuf, String> {
    Ok(home()?
        .join("Library")
        .join("Logs")
        .join("bosun")
        .join(format!("{name}.log")))
}

fn linux_file(name: &str) -> Result<PathBuf, String> {
    let dir = match std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        Some(config) => config,
        None => home()?.join(".config"),
    };
    Ok(dir
        .join("systemd")
        .join("user")
        .join(artefact::linux_unit(name)))
}

/// Writes the registration of `launch` and makes the service manager know it; starts
/// nothing. Returns what was done.
pub(crate) fn register(platform: Platform, launch: &Launch) -> Result<String, String> {
    match platform {
        Platform::Windows => {
            let xml = artefact::windows_task(launch, &windows_user()?);
            let file = std::env::temp_dir().join(format!(
                "bosun-task-{}-{}.xml",
                launch.name,
                std::process::id()
            ));
            write(&file, &artefact::utf16_file(&xml))?;
            let created = must(
                "schtasks",
                &[
                    os("/Create"),
                    os("/TN"),
                    os(&launch.name),
                    os("/XML"),
                    file.as_os_str(),
                    os("/F"),
                ],
            );
            let _ = std::fs::remove_file(&file);
            created?;
            Ok(format!(
                "registered the Task Scheduler task `{}`, run at your logon",
                launch.name
            ))
        }
        Platform::Macos => {
            let file = macos_file(&launch.name)?;
            write(&file, artefact::macos_plist(launch).as_bytes())?;
            // launchd opens the log file but does not create its directory.
            if let Some(dir) = macos_log(&launch.name)?.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            }
            // A label disabled earlier (`launchctl disable`) would not load.
            let _ = run(
                "launchctl",
                &[os("enable"), os(&macos_service(&launch.name)?)],
            );
            Ok(format!("wrote {}, run at your login", file.display()))
        }
        Platform::Linux => {
            let unit = artefact::linux_unit(&launch.name);
            let file = linux_file(&launch.name)?;
            write(&file, artefact::linux_unit_file(launch).as_bytes())?;
            must("systemctl", &[os("--user"), os("daemon-reload")])?;
            must("systemctl", &[os("--user"), os("enable"), os(&unit)])?;
            Ok(format!(
                "wrote and enabled {}, wanted by default.target",
                file.display()
            ))
        }
    }
}

/// Stops the daemon the registration `name` started, if it runs. Best effort: a registration
/// that is not loaded has nothing to stop.
pub(crate) fn stop(platform: Platform, name: &str) -> Result<(), String> {
    match platform {
        Platform::Windows => {
            let _ = run("schtasks", &[os("/End"), os("/TN"), os(name)]);
        }
        Platform::Macos => {
            let _ = run("launchctl", &[os("bootout"), os(&macos_service(name)?)]);
        }
        Platform::Linux => {
            let unit = artefact::linux_unit(name);
            let _ = run("systemctl", &[os("--user"), os("stop"), os(&unit)]);
        }
    }
    Ok(())
}

/// Ends process `pid` (Windows: `taskkill /F`). `schtasks /End` ends the task's
/// `conhost.exe`, not the `bosund` it hosts, so the daemon is stopped by the pid it answers
/// `hello` with.
pub(crate) fn kill_windows(pid: u32) -> Result<(), String> {
    must("taskkill", &[os("/PID"), os(&pid.to_string()), os("/F")]).map(|_| ())
}

/// Starts the daemon from the registration `name`.
pub(crate) fn start(platform: Platform, name: &str) -> Result<(), String> {
    match platform {
        Platform::Windows => {
            must("schtasks", &[os("/Run"), os("/TN"), os(name)])?;
        }
        Platform::Macos => {
            let domain = macos_domain()?;
            let file = macos_file(name)?;
            // `bootout` returns before the job is gone (it waits up to the job's exit timeout,
            // 20 s by default); `bootstrap` fails until it is.
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                match must(
                    "launchctl",
                    &[os("bootstrap"), os(&domain), file.as_os_str()],
                ) {
                    Ok(_) => break,
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Platform::Linux => {
            let unit = artefact::linux_unit(name);
            must("systemctl", &[os("--user"), os("start"), os(&unit)])?;
        }
    }
    Ok(())
}

/// Stops the daemon and removes the registration `name`. Returns what was done, one line
/// each; empty when nothing was registered.
pub(crate) fn uninstall(platform: Platform, name: &str) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    match platform {
        Platform::Windows => {
            if query(platform, name)?.is_some() {
                stop(platform, name)?;
                must("schtasks", &[os("/Delete"), os("/TN"), os(name), os("/F")])?;
                done.push(format!(
                    "stopped and removed the Task Scheduler task `{name}`"
                ));
            }
        }
        Platform::Macos => {
            let service = macos_service(name)?;
            if run("launchctl", &[os("bootout"), os(&service)])?
                .status
                .success()
            {
                done.push(format!("stopped and unloaded {service}"));
            }
            for file in [macos_file(name)?, macos_log(name)?] {
                if remove(&file)? {
                    done.push(format!("removed {}", file.display()));
                }
            }
        }
        Platform::Linux => {
            let file = linux_file(name)?;
            if file.exists() {
                let unit = artefact::linux_unit(name);
                // Without a user service manager there is nothing to stop; the file still goes.
                if run(
                    "systemctl",
                    &[os("--user"), os("disable"), os("--now"), os(&unit)],
                )?
                .status
                .success()
                {
                    done.push(format!("stopped and disabled {unit}"));
                }
                remove(&file)?;
                done.push(format!("removed {}", file.display()));
                let _ = run("systemctl", &[os("--user"), os("daemon-reload")]);
            }
        }
    }
    Ok(done)
}

/// The registration `name`, when there is one.
pub(crate) fn query(platform: Platform, name: &str) -> Result<Option<Registration>, String> {
    match platform {
        Platform::Windows => {
            let output = run("schtasks", &[os("/Query"), os("/TN"), os(name), os("/XML")])?;
            if !output.status.success() {
                return Ok(None);
            }
            let xml = String::from_utf8_lossy(&output.stdout);
            Ok(Some(Registration {
                place: format!("the Task Scheduler task `{name}`"),
                loaded: true,
                daemon: artefact::windows_task_daemon(&xml).map(PathBuf::from),
                path_var: None,
            }))
        }
        Platform::Macos => {
            let file = macos_file(name)?;
            let Ok(text) = std::fs::read_to_string(&file) else {
                return Ok(None);
            };
            let loaded = run("launchctl", &[os("print"), os(&macos_service(name)?)])?
                .status
                .success();
            Ok(Some(Registration {
                place: file.display().to_string(),
                loaded,
                daemon: artefact::macos_plist_daemon(&text).map(PathBuf::from),
                path_var: artefact::macos_plist_path_var(&text),
            }))
        }
        Platform::Linux => {
            let file = linux_file(name)?;
            let Ok(text) = std::fs::read_to_string(&file) else {
                return Ok(None);
            };
            let unit = artefact::linux_unit(name);
            let enabled = run("systemctl", &[os("--user"), os("is-enabled"), os(&unit)])
                .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "enabled");
            Ok(Some(Registration {
                place: file.display().to_string(),
                loaded: enabled,
                daemon: artefact::linux_unit_daemon(&text).map(PathBuf::from),
                path_var: artefact::linux_unit_path_var(&text),
            }))
        }
    }
}

/// `bosund` next to the running `bosun`.
pub(crate) fn daemon_next_to_self() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate bosun itself: {e}"))?;
    Ok(exe.with_file_name(format!("bosund{}", std::env::consts::EXE_SUFFIX)))
}

/// What the logon process runs: `daemon` with `--config`, `--endpoint` and `--operator-file` when
/// they are given (the logon process sees none of `BOSUN_CONFIG`, `BOSUN_ENDPOINT` nor
/// `BOSUN_OPERATOR_FILE`), and on launchd and systemd the current `PATH`, which they would
/// otherwise replace with a minimal one.
pub(crate) fn launch(
    platform: Platform,
    name: &str,
    daemon: &Path,
    config: Option<&Path>,
    endpoint: Option<&str>,
    operator_file: Option<&Path>,
) -> Result<Launch, String> {
    let text = |path: &Path| -> Result<String, String> {
        let absolute = std::path::absolute(path)
            .map_err(|e| format!("cannot make {} absolute: {e}", path.display()))?;
        absolute
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("{} is not valid UTF-8", absolute.display()))
    };
    if !daemon.is_file() {
        return Err(format!(
            "no daemon at {}: build bosund next to bosun, or pass --daemon <path>",
            daemon.display()
        ));
    }
    let mut args = Vec::new();
    if let Some(config) = config {
        args.push("--config".to_owned());
        args.push(text(config)?);
    }
    if let Some(endpoint) = endpoint {
        args.push("--endpoint".to_owned());
        args.push(endpoint.to_owned());
    }
    if let Some(operator_file) = operator_file {
        args.push("--operator-file".to_owned());
        args.push(text(operator_file)?);
    }
    let (path_var, log_file) = match platform {
        Platform::Windows => (None, None),
        Platform::Macos => (std::env::var("PATH").ok(), Some(text(&macos_log(name)?)?)),
        Platform::Linux => (std::env::var("PATH").ok(), None),
    };
    Ok(Launch {
        name: name.to_owned(),
        daemon: text(daemon)?,
        args,
        path_var,
        log_file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_restricted() {
        assert_eq!(
            parse_name("bosun-test_1.x").as_deref(),
            Ok("bosun-test_1.x")
        );
        for bad in ["", ".hidden", "a b", r"a\b", "a/b", &"x".repeat(65)] {
            assert!(parse_name(bad).is_err(), "{bad:?}");
        }
    }
}

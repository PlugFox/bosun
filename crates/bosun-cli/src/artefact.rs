//! The artefacts that register the daemon as a logon process (#6, #28): a Task Scheduler task
//! (Windows), a `LaunchAgent` property list (macOS) and a systemd user unit (Linux).
//!
//! Pure: each generator takes paths and options and returns text, so every platform's artefact
//! is tested on every host. Writing and loading them is `install.rs`.

use std::fmt::Write as _;
use std::path::Path;

/// What the logon process runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    /// The registration's name: the task's name, the unit's stem, the last part of the label.
    pub(crate) name: String,
    /// The absolute path of `bosund`.
    pub(crate) daemon: String,
    /// Its arguments.
    pub(crate) args: Vec<String>,
    /// `PATH` for the daemon, where the service manager would otherwise give it a minimal one
    /// (launchd, systemd). `None` leaves the manager's.
    pub(crate) path_var: Option<String>,
    /// The file launchd sends the daemon's stdout and stderr to; systemd keeps them in the
    /// journal, and Task Scheduler drops them.
    pub(crate) log_file: Option<String>,
}

/// The `LaunchAgent` label of a registration: reverse DNS of the repository's home.
pub(crate) fn macos_label(name: &str) -> String {
    format!("io.github.plugfox.{name}")
}

/// The systemd unit file name of a registration.
pub(crate) fn linux_unit(name: &str) -> String {
    format!("{name}.service")
}

/// Escapes text for an XML element or attribute.
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The inverse of [`xml_escape`] for the entities it writes.
fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Quotes one argument for a Windows command line, so that `CommandLineToArgvW` and the Rust
/// runtime read it back unchanged.
pub(crate) fn windows_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// The Task Scheduler task (`schtasks /Create /XML`): started at the logon of `user`
/// (`DOMAIN\name`), as that user, without elevation, with no time limit and one instance at a
/// time.
///
/// `bosund` is a console program, and a console program started at logon opens a window that
/// closing would kill the daemon with. The action therefore runs it through
/// `conhost.exe --headless`, which gives it a console that is never shown.
pub(crate) fn windows_task(launch: &Launch, user: &str) -> String {
    let user = xml_escape(user);
    let mut arguments = String::from("--headless ");
    arguments.push_str(&windows_quote(&launch.daemon));
    for arg in &launch.args {
        arguments.push(' ');
        arguments.push_str(&windows_quote(arg));
    }
    // Mutant `mutant_install_boot_trigger` (#28): the task starts at boot instead of at the
    // user's logon.
    let trigger = if cfg!(mutant_install_boot_trigger) {
        "    <BootTrigger>\n      <Enabled>true</Enabled>\n    </BootTrigger>\n".to_owned()
    } else {
        format!(
            "    <LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>{user}</UserId>\n    \
             </LogonTrigger>\n"
        )
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
  <RegistrationInfo>\n\
    <Description>bosun: starts bosund at logon (bosun install)</Description>\n\
    <URI>\\{name}</URI>\n\
  </RegistrationInfo>\n\
  <Triggers>\n\
{trigger}\
  </Triggers>\n\
  <Principals>\n\
    <Principal id=\"Author\">\n\
      <UserId>{user}</UserId>\n\
      <LogonType>InteractiveToken</LogonType>\n\
      <RunLevel>LeastPrivilege</RunLevel>\n\
    </Principal>\n\
  </Principals>\n\
  <Settings>\n\
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
    <AllowHardTerminate>true</AllowHardTerminate>\n\
    <StartWhenAvailable>false</StartWhenAvailable>\n\
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n\
    <IdleSettings>\n\
      <StopOnIdleEnd>false</StopOnIdleEnd>\n\
      <RestartOnIdle>false</RestartOnIdle>\n\
    </IdleSettings>\n\
    <AllowStartOnDemand>true</AllowStartOnDemand>\n\
    <Enabled>true</Enabled>\n\
    <Hidden>false</Hidden>\n\
    <RunOnlyIfIdle>false</RunOnlyIfIdle>\n\
    <WakeToRun>false</WakeToRun>\n\
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n\
    <Priority>5</Priority>\n\
  </Settings>\n\
  <Actions Context=\"Author\">\n\
    <Exec>\n\
      <Command>%SystemRoot%\\System32\\conhost.exe</Command>\n\
      <Arguments>{arguments}</Arguments>\n\
    </Exec>\n\
  </Actions>\n\
</Task>\n",
        name = xml_escape(&launch.name),
        arguments = xml_escape(&arguments),
    )
}

/// The task as `schtasks /XML` reads it: UTF-16, little endian, with a byte-order mark, as
/// its declaration says.
pub(crate) fn utf16_file(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// The `LaunchAgent` property list: loaded at login into the user's GUI domain and run at load.
pub(crate) fn macos_plist(launch: &Launch) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n",
    );
    let _ = writeln!(
        out,
        "  <key>Label</key>\n  <string>{}</string>",
        xml_escape(&macos_label(&launch.name))
    );
    out.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for arg in std::iter::once(&launch.daemon).chain(&launch.args) {
        let _ = writeln!(out, "    <string>{}</string>", xml_escape(arg));
    }
    out.push_str("  </array>\n");
    if let Some(path) = &launch.path_var {
        let _ = writeln!(
            out,
            "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key>\n    \
             <string>{}</string>\n  </dict>",
            xml_escape(path)
        );
    }
    if let Some(log) = &launch.log_file {
        let log = xml_escape(log);
        let _ = writeln!(
            out,
            "  <key>StandardOutPath</key>\n  <string>{log}</string>\n  \
             <key>StandardErrorPath</key>\n  <string>{log}</string>"
        );
    }
    out.push_str(
        "  <key>RunAtLoad</key>\n  <true/>\n\
  <key>ProcessType</key>\n  <string>Background</string>\n\
</dict>\n\
</plist>\n",
    );
    out
}

/// `PATH` as a property list written by [`macos_plist`] sets it.
pub(crate) fn macos_plist_path_var(plist: &str) -> Option<String> {
    let rest = &plist[plist.find("<key>PATH</key>")?..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")? + start;
    Some(xml_unescape(&rest[start..end]))
}

/// Quotes one word for a systemd unit file: double quotes with C escapes; `%` (a specifier)
/// doubled; `$` (a variable in `ExecStart=`) doubled.
fn systemd_quote(word: &str) -> String {
    let mut out = String::from("\"");
    for c in word.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The inverse of [`systemd_quote`] for one quoted word.
fn systemd_unquote(word: &str) -> Option<String> {
    let inner = word.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                other => out.push(other),
            },
            '%' | '$' => {
                chars.next()?;
                out.push(c);
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// The systemd user unit: wanted by `default.target`, which the user's service manager reaches
/// at login.
pub(crate) fn linux_unit_file(launch: &Launch) -> String {
    let mut exec = systemd_quote(&launch.daemon);
    for arg in &launch.args {
        exec.push(' ');
        exec.push_str(&systemd_quote(arg));
    }
    let mut out = String::from(
        "[Unit]\n\
Description=bosun daemon (bosund), registered by bosun install\n\
Documentation=https://github.com/PlugFox/bosun\n\
\n\
[Service]\n\
Type=simple\n",
    );
    let _ = writeln!(out, "ExecStart={exec}");
    if let Some(path) = &launch.path_var {
        let _ = writeln!(
            out,
            "Environment={}",
            systemd_quote(&format!("PATH={path}"))
        );
    }
    out.push_str("\n[Install]\nWantedBy=default.target\n");
    out
}

/// `PATH` as a unit written by [`linux_unit_file`] sets it.
pub(crate) fn linux_unit_path_var(unit: &str) -> Option<String> {
    let line = unit
        .lines()
        .find_map(|line| line.strip_prefix("Environment="))?;
    systemd_unquote(line)?
        .strip_prefix("PATH=")
        .map(str::to_owned)
}

/// Reads back the first argument of a Windows command line written by [`windows_quote`], and
/// the rest of the line.
fn windows_first_arg(line: &str) -> Option<(String, &str)> {
    let line = line.trim_start();
    let Some(quoted) = line.strip_prefix('"') else {
        let end = line.find([' ', '\t']).unwrap_or(line.len());
        return (end > 0).then(|| (line[..end].to_owned(), &line[end..]));
    };
    let mut out = String::new();
    let mut backslashes = 0;
    for (at, c) in quoted.char_indices() {
        match c {
            '\\' => backslashes += 1,
            '"' if backslashes % 2 == 1 => {
                out.push_str(&"\\".repeat(backslashes / 2));
                out.push('"');
                backslashes = 0;
            }
            '"' => {
                out.push_str(&"\\".repeat(backslashes / 2));
                return Some((out, &quoted[at + 1..]));
            }
            c => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    None
}

/// The daemon a task written by [`windows_task`] runs, from its XML as `schtasks /Query /XML`
/// prints it.
pub(crate) fn windows_task_daemon(xml: &str) -> Option<String> {
    let start = xml.find("<Arguments>")? + "<Arguments>".len();
    let end = xml[start..].find("</Arguments>")? + start;
    let arguments = xml_unescape(&xml[start..end]);
    let (first, rest) = windows_first_arg(&arguments)?;
    if first == "--headless" {
        Some(windows_first_arg(rest)?.0)
    } else {
        Some(first)
    }
}

/// The daemon a property list written by [`macos_plist`] runs.
pub(crate) fn macos_plist_daemon(plist: &str) -> Option<String> {
    let rest = &plist[plist.find("<key>ProgramArguments</key>")?..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")? + start;
    Some(xml_unescape(&rest[start..end]))
}

/// The daemon a unit written by [`linux_unit_file`] runs.
pub(crate) fn linux_unit_daemon(unit: &str) -> Option<String> {
    let line = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))?;
    // The first quoted word: up to the first quote that no backslash escapes.
    let mut escaped = false;
    for (at, c) in line.char_indices().skip(1) {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return systemd_unquote(&line[..=at]),
            _ => escaped = false,
        }
    }
    None
}

/// Where a `LaunchAgent` of `label` lives under `home`.
pub(crate) fn macos_plist_file(home: &Path, label: &str) -> std::path::PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{label}.plist"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> Launch {
        Launch {
            name: "bosun".to_owned(),
            daemon: "/opt/bosun & co/bosund".to_owned(),
            args: vec![
                "--config".to_owned(),
                "/home/u/my config/100%$x.toml".to_owned(),
            ],
            path_var: Some("/usr/local/bin:/usr/bin:/bin".to_owned()),
            log_file: Some("/home/u/Library/Logs/bosun/bosun.log".to_owned()),
        }
    }

    #[test]
    fn the_registered_daemon_is_read_back() {
        let launch = launch();
        assert_eq!(
            windows_task_daemon(&windows_task(&launch, r"HOST\someone")).as_deref(),
            Some(launch.daemon.as_str())
        );
        assert_eq!(
            macos_plist_daemon(&macos_plist(&launch)).as_deref(),
            Some(launch.daemon.as_str())
        );
        assert_eq!(
            linux_unit_daemon(&linux_unit_file(&launch)).as_deref(),
            Some(launch.daemon.as_str())
        );
        for odd in [r"C:\Program Files\b\bosund.exe", r#"C:\a "b"\c\"#, "plain"] {
            let line = format!("{} --config x", windows_quote(odd));
            assert_eq!(
                windows_first_arg(&line).map(|(a, _)| a).as_deref(),
                Some(odd)
            );
        }
        let odd = Launch {
            daemon: "/a \"q\" \\ %$/bosund".to_owned(),
            ..launch
        };
        assert_eq!(
            linux_unit_daemon(&linux_unit_file(&odd)).as_deref(),
            Some(odd.daemon.as_str())
        );
    }

    /// The triggers of #28's acceptance: each artefact starts the daemon at the user's logon,
    /// never at boot or for another user.
    #[test]
    fn each_artefact_names_a_logon_trigger() {
        let task = windows_task(&launch(), r"HOST\someone");
        let triggers = &task[task.find("<Triggers>").unwrap()..task.find("</Triggers>").unwrap()];
        assert!(
            triggers.contains("<LogonTrigger>")
                && triggers.contains(r"<UserId>HOST\someone</UserId>"),
            "{task}"
        );
        assert!(!task.contains("<BootTrigger>"), "{task}");
        assert!(
            task.contains("<LogonType>InteractiveToken</LogonType>"),
            "{task}"
        );
        assert!(
            task.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
            "{task}"
        );

        let plist = macos_plist(&launch());
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"), "{plist}");
        assert!(
            plist.contains("<string>io.github.plugfox.bosun</string>"),
            "{plist}"
        );

        let unit = linux_unit_file(&launch());
        assert!(
            unit.contains("[Install]\nWantedBy=default.target\n"),
            "{unit}"
        );
    }

    #[test]
    fn the_daemon_and_its_arguments_survive_quoting() {
        let task = windows_task(&launch(), r"HOST\someone");
        assert!(
            task.contains(
                "<Arguments>--headless &quot;/opt/bosun &amp; co/bosund&quot; --config \
                 &quot;/home/u/my config/100%$x.toml&quot;</Arguments>"
            ),
            "{task}"
        );

        let plist = macos_plist(&launch());
        assert!(
            plist.contains(
                "    <string>/opt/bosun &amp; co/bosund</string>\n    <string>--config</string>\n"
            ),
            "{plist}"
        );
        assert_eq!(
            macos_plist_path_var(&plist).as_deref(),
            Some("/usr/local/bin:/usr/bin:/bin")
        );

        let unit = linux_unit_file(&launch());
        assert!(
            unit.contains(
                "ExecStart=\"/opt/bosun & co/bosund\" \"--config\" \
                 \"/home/u/my config/100%%$$x.toml\"\n"
            ),
            "{unit}"
        );
        assert_eq!(
            linux_unit_path_var(&unit).as_deref(),
            Some("/usr/local/bin:/usr/bin:/bin")
        );
        let odd = "a\"b\\c%d$e";
        assert_eq!(systemd_unquote(&systemd_quote(odd)).as_deref(), Some(odd));

        assert!(
            plist.contains(
                "<key>StandardErrorPath</key>\n  <string>/home/u/Library/Logs/bosun/bosun.log"
            ),
            "{plist}"
        );
        let mut bare = launch();
        bare.path_var = None;
        assert_eq!(macos_plist_path_var(&macos_plist(&bare)), None);
        assert_eq!(linux_unit_path_var(&linux_unit_file(&bare)), None);
    }

    #[test]
    fn windows_arguments_are_quoted_as_argv_reads_them() {
        assert_eq!(windows_quote("plain"), "plain");
        assert_eq!(windows_quote(""), "\"\"");
        assert_eq!(
            windows_quote(r"C:\Program Files\bosun\bosund.exe"),
            r#""C:\Program Files\bosun\bosund.exe""#
        );
        assert_eq!(
            windows_quote(r"C:\dir with space\"),
            r#""C:\dir with space\\""#
        );
        assert_eq!(windows_quote(r#"say "hi""#), r#""say \"hi\"""#);
    }

    #[test]
    fn the_task_file_is_utf16_with_a_byte_order_mark() {
        let bytes = utf16_file("<é>");
        assert_eq!(bytes, [0xFF, 0xFE, b'<', 0, 0xE9, 0, b'>', 0]);
    }
}

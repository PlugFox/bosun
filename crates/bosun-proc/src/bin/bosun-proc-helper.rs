//! The child of the shim tests (#8): `bosun-proc-helper ROLE`, one role per behaviour.
//!
//! - `sleep`: sleeps for a minute.
//! - `parent-stay`: starts a sleeping grandchild, reports its pid on stdout as
//!   `bosun-proc-helper grandchild <pid>`, then sleeps.
//! - `speak`: reports a line on stdout and one on stderr, then exits with 3.
//!
//! The helper has no other use: it exists so the tests run a real child without relying on
//! any program of the machine.

use std::process::{Command, Stdio};
use std::time::Duration;

/// Far beyond any test, short enough that a leaked helper ends.
const SLEEP: Duration = Duration::from_secs(60);

fn report(key: &str, value: impl std::fmt::Display) {
    println!("bosun-proc-helper {key} {value}");
}

fn main() {
    let role = std::env::args().nth(1).unwrap_or_default();
    match role.as_str() {
        "sleep" => std::thread::sleep(SLEEP),
        "parent-stay" => {
            #[expect(
                clippy::zombie_processes,
                reason = "the grandchild must outlive its parent; its new parent reaps it"
            )]
            let grandchild = Command::new(std::env::current_exe().expect("current exe"))
                .arg("sleep")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn the grandchild");
            report("grandchild", grandchild.id());
            std::thread::sleep(SLEEP);
        }
        "speak" => {
            report("greeting", "hello");
            eprintln!("bosun-proc-helper alarm bell");
            std::process::exit(3);
        }
        other => {
            eprintln!("bosun-proc-helper: unknown role {other:?}");
            std::process::exit(2);
        }
    }
}

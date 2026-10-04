//! Process-level checks: the service reports its version, stays alive,
//! and exits cleanly on SIGTERM.

use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

fn belvedered() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_belvedered"));
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

#[test]
fn version_flag_prints_name_and_version_and_exits() {
    let out = belvedered().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("belvedered {}", belvedere_core::VERSION)
    );
}

#[test]
fn stays_alive_then_exits_cleanly_on_sigterm() {
    let mut child = belvedered().spawn().unwrap();

    // Give it a moment and confirm it has not exited on its own.
    sleep(Duration::from_millis(500));
    assert!(child.try_wait().unwrap().is_none(), "service exited early");

    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "did not exit within 5s of SIGTERM"
        );
        sleep(Duration::from_millis(50));
    };

    assert!(status.success(), "expected a clean exit, got {status}");

    let mut stderr = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    // Without journald (as in CI) logs land on stderr; with it, stderr is quiet.
    if !stderr.is_empty() {
        assert!(stderr.contains("shutting down"), "stderr was: {stderr}");
    }
}

//! The shipped systemd unit says what the plan requires.

const UNIT: &str = include_str!("../../../packaging/systemd/belvedere.service");

fn value(key: &str) -> Option<&'static str> {
    UNIT.lines()
        .find_map(|line| {
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix('='))
        })
        .map(str::trim)
}

#[test]
fn restarts_on_failure_quickly() {
    assert_eq!(value("Restart"), Some("on-failure"));
    let secs: u64 = value("RestartSec").unwrap().parse().unwrap();
    assert!(secs <= 3, "RestartSec must leave room to be back within 5s");
}

#[test]
fn starts_at_login_and_runs_the_installed_binary() {
    assert_eq!(value("WantedBy"), Some("default.target"));
    assert_eq!(value("ExecStart"), Some("%h/.local/bin/belvedered"));
}

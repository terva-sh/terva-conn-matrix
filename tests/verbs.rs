//! Lifecycle-verb smokes: configured/status/reset/verify against an
//! isolated $TERVA_HOME (the host gives these verbs 5 s and no tty).
//!
//! `verify` is the exception on timing: terva never calls it, so it may
//! take as long as an operator will wait. The smokes below keep it offline
//! or unconfigured, so none of them spend that budget.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn temp_home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("terva-conn-matrix-verbs-{name}"));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).expect("create test TERVA_HOME");
    home
}

fn run_verb(home: &Path, verb: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terva-conn-matrix"))
        .arg(verb)
        .env("TERVA_HOME", home)
        .output()
        .expect("run verb")
}

fn stdout_of(out: Output) -> String {
    String::from_utf8(out.stdout).expect("utf8")
}

fn write_config(home: &Path, json: &str) {
    let state_dir = home.join("connectors/matrix");
    fs::create_dir_all(&state_dir).expect("state dir");
    fs::write(state_dir.join("config.json"), json).expect("write config");
}

fn seed_session(home: &Path) {
    let state_dir = home.join("connectors/matrix");
    fs::create_dir_all(&state_dir).expect("state dir");
    fs::write(
        state_dir.join("config.json"),
        br#"{"homeserver_url":"https://matrix.example.org","user_id":"@bot:example.org","device_id":"ABCDEFG","auto_join":"always","max_attachment_mb":64}"#,
    )
    .expect("write config");
    fs::write(
        state_dir.join("session.json"),
        br#"{"user_id":"@bot:example.org","device_id":"ABCDEFG","access_token":"syt_verysecrettokenvalue_123"}"#,
    )
    .expect("write session");
}

#[test]
fn configured_is_a_pure_predicate() {
    let home = temp_home("configured");
    // Fresh home: not configured, exit 1, no stdout.
    let out = run_verb(&home, "configured");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "configured prints nothing");
    // With a session: exit 0.
    seed_session(&home);
    let out = run_verb(&home, "configured");
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
}

#[test]
fn status_masks_the_token() {
    let home = temp_home("status");
    let out = run_verb(&home, "status");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).expect("utf8");
    assert!(text.contains("not configured"), "got: {text}");

    seed_session(&home);
    let out = run_verb(&home, "status");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).expect("utf8");
    assert!(text.contains("@bot:example.org"), "got: {text}");
    assert!(text.contains("https://matrix.example.org"), "got: {text}");
    assert!(
        !text.contains("syt_verysecrettokenvalue_123"),
        "status must never print the full token: {text}"
    );
    assert!(
        text.contains("syt_..._123"),
        "masked shape expected: {text}"
    );
}

#[test]
fn reset_wipes_local_state_and_only_ours() {
    let home = temp_home("reset");
    seed_session(&home);
    let state_dir = home.join("connectors/matrix");
    // An unreachable local homeserver makes the best-effort logout fail
    // instantly instead of waiting out DNS/timeouts.
    fs::write(
        state_dir.join("config.json"),
        br#"{"homeserver_url":"https://127.0.0.1:9","user_id":"@bot:example.org","device_id":"ABCDEFG"}"#,
    )
    .expect("rewrite config");
    // Host-owned files reset must NEVER touch.
    fs::write(state_dir.join("pairing.json"), b"{}").expect("pairing");
    fs::create_dir_all(state_dir.join("data")).expect("data dir");
    fs::write(state_dir.join("data/attachment.png"), b"x").expect("staged file");

    // Server-side logout will fail (nothing at example.org) — reset reports
    // it to stderr and still wipes local state.
    let out = run_verb(&home, "reset");
    assert_eq!(out.status.code(), Some(0));
    assert!(!state_dir.join("session.json").exists());
    assert!(!state_dir.join("config.json").exists());
    assert!(!state_dir.join("store").exists());
    assert!(
        state_dir.join("pairing.json").exists(),
        "pairing is host-owned"
    );
    assert!(
        state_dir.join("data/attachment.png").exists(),
        "data/ is host staging"
    );

    // Idempotent: nothing left to remove is still success.
    let out = run_verb(&home, "reset");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).expect("utf8");
    assert!(text.contains("no matrix config to remove"), "got: {text}");
}

#[test]
fn unknown_verb_exits_2() {
    let home = temp_home("unknown");
    let out = run_verb(&home, "frobnicate");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn verify_is_a_known_verb_but_needs_a_session() {
    let home = temp_home("verify-unconfigured");
    let out = run_verb(&home, "verify");
    // Exit 1, not the exit 2 an unknown verb gets: the verb dispatches, it
    // just has nothing to check.
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stdout.is_empty(),
        "the verdict goes to stdout or nowhere"
    );
    let err = String::from_utf8(out.stderr).expect("utf8");
    assert!(err.contains("not configured"), "got: {err}");
}

#[test]
fn status_does_not_call_a_missing_verdict_unverified() {
    let home = temp_home("verdict-missing");
    seed_session(&home);
    // seed_session writes no e2ee field, which is exactly what a setup
    // killed before its final save leaves behind. The old wording called
    // this "unknown", which reads as a verdict about the device rather
    // than the absence of one.
    let text = stdout_of(run_verb(&home, "status"));
    assert!(text.contains("not recorded"), "got: {text}");
    assert!(text.contains("verify"), "it must name the way out: {text}");
}

#[test]
fn status_says_where_the_verdict_came_from() {
    let home = temp_home("verdict-source");
    seed_session(&home);

    // A config written before e2ee_source existed carries none, and
    // everything that wrote one back then was setup.
    write_config(
        &home,
        r#"{"homeserver_url":"https://matrix.example.org","user_id":"@bot:example.org","device_id":"ABCDEFG","e2ee":"recovery enabled, device verified"}"#,
    );
    let text = stdout_of(run_verb(&home, "status"));
    assert!(text.contains("(as of setup)"), "got: {text}");

    write_config(
        &home,
        r#"{"homeserver_url":"https://matrix.example.org","user_id":"@bot:example.org","device_id":"ABCDEFG","e2ee":"device verified","e2ee_source":"verify"}"#,
    );
    let text = stdout_of(run_verb(&home, "status"));
    assert!(text.contains("(as of the last verify)"), "got: {text}");
}

#[test]
fn verify_records_a_verdict_even_when_the_homeserver_is_unreachable() {
    let home = temp_home("verify-offline");
    seed_session(&home);
    // Nothing listens on port 9, so every sync fails at once and
    // observe_verdict falls back to what the (empty) crypto store says.
    write_config(
        &home,
        r#"{"homeserver_url":"https://127.0.0.1:9","user_id":"@bot:example.org","device_id":"ABCDEFG"}"#,
    );

    let out = run_verb(&home, "verify");
    assert_eq!(
        out.status.code(),
        Some(0),
        "an offline read is still a read"
    );
    let text = stdout_of(out);
    assert!(text.contains("ABCDEFG"), "got: {text}");

    // The point of the verb: the reading reaches disk. Without this the
    // operator is back where the killed setup left them.
    let text = stdout_of(run_verb(&home, "status"));
    assert!(
        text.contains("(as of the last verify)"),
        "verify must claim its own writes: {text}"
    );
    assert!(!text.contains("not recorded"), "got: {text}");
}

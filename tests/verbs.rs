//! Lifecycle-verb smokes: configured/status/reset against an isolated
//! $TERVA_HOME (the host gives these verbs 5 s and no tty).

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

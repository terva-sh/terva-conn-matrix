//! terva Matrix connector.
//!
//! A standalone external connector speaking terva's connector protocol
//! (connproto v2) over stdio: host→connector on stdin, connector→host on
//! stdout, one JSON object per LF line, diagnostics on stderr only.
//!
//! terva invokes the manifest `exec` with a lifecycle verb appended as the
//! LAST argv element: run | setup | status | reset | configured. Only `run`
//! speaks the protocol.
//!
//! `verify` is ours rather than the host's: terva never calls it, and an
//! operator runs it by hand against a configured home. It re-reads the
//! E2EE verdict on the existing session, which `status` cannot do because
//! it only reports the stored snapshot.

use std::io;
use std::process::ExitCode;

use terva_conn_matrix::matrix::MatrixService;
use terva_conn_matrix::proto::Capabilities;
use terva_conn_matrix::serve::{serve, ServeConfig};
use terva_conn_matrix::setup;

/// Kept equal to Cargo.toml's `version` and connector.json's `version` by the
/// lockstep test in tests/conventions.rs; bump all three together.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// v2 message identity (chat-scoped ids, result.message_id) is load-bearing
/// for the Matrix mapping — we do not implement the v1 reply_to conflation.
const PROTOCOL_MIN: i64 = 2;
const PROTOCOL_MAX: i64 = 2;

fn main() -> ExitCode {
    // The lifecycle verb is the LAST argv element (manifest args come before
    // it); a bare manual launch has only the program name there.
    let argv: Vec<String> = std::env::args().collect();
    let verb = argv.last().map(String::as_str).unwrap_or("");
    match verb {
        "run" => run(),
        "setup" => setup::setup(),
        "status" => setup::status(),
        "reset" => setup::reset(),
        "configured" => setup::configured(),
        "verify" => setup::verify(),
        other => {
            eprintln!(
                "[matrix] unknown verb {other:?} — expected run|setup|status|reset|configured|verify as the last argument"
            );
            ExitCode::from(2)
        }
    }
}

fn run() -> ExitCode {
    // A panic must land in the host-captured log with a backtrace, not
    // vanish; the process still dies non-zero (restart budget applies).
    terva_env::install_panic_hook("matrix");
    init_tracing();
    // The first line of every session's log: what is running, from where.
    // Diagnosis starts here; RUST_LOG=debug adds the full wire trace and
    // matrix_sdk internals (e.g. RUST_LOG=debug,matrix_sdk_crypto=trace).
    tracing::info!(
        version = VERSION,
        protocol = format_args!("{PROTOCOL_MIN}..{PROTOCOL_MAX}"),
        state_dir = %terva_conn_matrix::config::state_dir().display(),
        "terva-conn-matrix starting"
    );
    // Declared features never run ahead of their implementation.
    // `message_ids`/`chat_kinds` are informative; `entities` and
    // `chat_membership` (P4) feed the host's group-admission gate;
    // message events and attachments are P5; the reaction-widget
    // `asks` are P6; `threads_out` is P7. min_edit_interval_ms stays
    // conservative — homeserver rate limits (429) hit the edit stream
    // first.
    let mut features = vec![
        "message_ids".to_string(),
        "chat_kinds".to_string(),
        "chat_parents".to_string(),
        "entities".to_string(),
        "chat_membership".to_string(),
        "edits_in".to_string(),
        "edits_out".to_string(),
        "deletes_in".to_string(),
        "deletes_out".to_string(),
        "reactions_in".to_string(),
        "reactions_out".to_string(),
        "attachment_kinds".to_string(),
        "asks".to_string(),
        "threads_out".to_string(),
        "typing_stop".to_string(),
    ];
    // MSC4144 per-message speaker profiles are config-gated (P8): with the
    // flag off (the default — client rendering is not yet universal) we
    // simply don't declare, and the host's `**Name:**` fallback does the
    // work. Missing config reads as the default, so the unconfigured hello
    // stays correct.
    let state_dir = terva_conn_matrix::config::state_dir();
    // Sweep a pre-0.13 session.json into the sealed config.json BEFORE the
    // hello declares the secret paths: the declaration is what the host's
    // per-read gate trusts, and a plaintext token in an undeclared sibling
    // file would make it a lie by omission. Failure is loud but not fatal —
    // an existing install must not go dark over a stale legacy file.
    if let Err(err) = terva_conn_matrix::config::migrate_legacy_session(&state_dir) {
        eprintln!("[matrix] WARNING: could not migrate the legacy session.json: {err}");
    }
    match terva_conn_matrix::config::load_config(&state_dir) {
        Ok(cfg) => features.extend(cfg.speaker_feature().map(String::from)),
        Err(err) => eprintln!("[matrix] ignoring unreadable config for capabilities: {err}"),
    }
    let config = ServeConfig {
        name: "matrix".into(),
        version: VERSION.into(),
        protocol_min: PROTOCOL_MIN,
        protocol_max: PROTOCOL_MAX,
        capabilities: Capabilities {
            max_text_len: 24000,
            typing_refresh_ms: 20000,
            sends_images: true,
            sends_files: true,
            min_edit_interval_ms: 1000,
            features,
        },
        // Declares our recipient + SECRET_PATHS in the hello, so terva can
        // re-seal our file during a key rotation without holding our key.
        // The SDK stays silent until a sealed save has minted the key — a
        // recipient we do not have would register a component terva could
        // never re-seal to.
        secrets: Some(terva_conn_matrix::config::sealed_state(&state_dir)),
    };
    let service = match MatrixService::new() {
        Ok(service) => service,
        Err(err) => {
            eprintln!("[matrix] fatal: cannot start the service runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match serve(&config, service) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // A dead wire is fatal by contract: exit promptly so the host's
            // restart budget applies.
            eprintln!("[matrix] fatal: {err}");
            ExitCode::FAILURE
        }
    }
}

/// tracing → stderr, always (stdout is the wire). The host captures stderr
/// to `$TERVA_HOME/logs/connector-matrix.log`.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,matrix_sdk=warn,matrix_sdk_crypto=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();
}

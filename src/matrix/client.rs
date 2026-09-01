//! Client construction, session restore, and the connect-time initial sync.

use std::path::Path;
use std::time::Duration;

use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::{RequestConfig, SyncSettings};
use matrix_sdk::encryption::{BackupDownloadStrategy, EncryptionSettings};
use matrix_sdk::{Client, ClientBuildError};

use crate::config::{store_path, Config};
use crate::serve::ConnectError;

use super::is_invalid_token;

/// Build a client. `store` = the sqlite state+crypto store dir; `None` keeps
/// everything in memory (used by `reset` for a one-shot logout).
///
/// The 10 s request timeout is safe for sync long-polls: the SDK adds the
/// sync timeout on top of the base request timeout for sync requests.
///
/// The error is boxed: `ClientBuildError` is ~160 bytes, and clippy 1.98's
/// `result_large_err` rightly objects to paying that on every Ok(Client)
/// return. This runs once per connect, so the box costs nothing that
/// matters.
pub async fn build_client(
    homeserver_url: &str,
    store: Option<&Path>,
) -> Result<Client, Box<ClientBuildError>> {
    let mut builder = Client::builder()
        .homeserver_url(homeserver_url)
        .request_config(
            // Rate limits (PLAN.md §8): the SDK classifies 429/M_LIMIT_EXCEEDED
            // as transient and honors the server's Retry-After when scheduling
            // these retries, on every REST path — combined with the declared
            // min_edit_interval_ms this is our 429 posture. A request still
            // limited after the retries surfaces as an honest error result.
            RequestConfig::new()
                .retry_limit(2)
                .timeout(Duration::from_secs(10)),
        )
        // NO auto cross-signing/backup enablement: on an account that
        // already has an identity those flows are destructive (they mint a
        // replacement identity). setup drives them explicitly by user
        // choice. Downloading backed-up keys after a decryption failure is
        // the one safe, always-helpful automation.
        .with_encryption_settings(EncryptionSettings {
            auto_enable_cross_signing: false,
            auto_enable_backups: false,
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
        });
    if let Some(path) = store {
        builder = builder.sqlite_store(path, None);
    }
    builder.build().await.map_err(Box::new)
}

/// The marker recording that the one-time history-discarding sync has run.
/// While it exists, restarts RESUME from the store's persisted sync token
/// instead — messages that arrived while the connector was down are
/// delivered on reconnect (dogfood finding: a restart-cycle that silently
/// eats messages reads as a broken bridge, not as discipline).
pub(crate) fn initial_sync_marker(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("initial-sync-done")
}

/// Restore the session and prepare the sync starting point.
///
/// First-ever connect: run the history-discarding initial sync — its
/// timeline is processed with no event handlers attached, so a freshly
/// set-up bot never replays pre-setup history into agent turns — and
/// return `Some(next_batch)` for the live loop. Every later connect
/// returns `None`: the live loop resumes from the token the store
/// persisted as the previous session processed its last sync, so downtime
/// is recovered, bounded by the server's limited-timeline window (a long
/// outage delivers the most recent events per room, not the whole gap).
pub(crate) async fn connect(
    config: &Config,
    state_dir: &Path,
    session: MatrixSession,
) -> Result<(Client, Option<String>), ConnectError> {
    if config.homeserver_url.is_empty() {
        return Err(ConnectError::Permanent(
            "config has no homeserver_url — run `terva bot setup --connector matrix`".into(),
        ));
    }
    let client = build_client(&config.homeserver_url, Some(&store_path(state_dir)))
        .await
        .map_err(|err| classify_build_error(*err))?;
    client.restore_session(session).await.map_err(|err| {
        // Local store/session mismatch — a respawn will hit it again; only
        // reset + setup cures it. That is "permanently broken" on the wire.
        ConnectError::Permanent(format!(
            "cannot restore the matrix session (run `reset` then `setup`): {err}"
        ))
    })?;

    if initial_sync_marker(state_dir).exists() {
        // Warm start. Validate the token with a cheap authenticated call —
        // the same dead-credential classification the sync used to give us.
        return match client.whoami().await {
            Ok(_) => {
                tracing::info!(
                    "warm start: resuming from the store's sync token (downtime will be recovered)"
                );
                Ok((client, None))
            }
            Err(err) => {
                let err: matrix_sdk::Error = err.into();
                if is_invalid_token(&err) {
                    Err(ConnectError::Permanent(
                        "homeserver rejected the stored access token (run `reset` then `setup`)"
                            .into(),
                    ))
                } else {
                    Err(ConnectError::Fatal(format!("token check failed: {err}")))
                }
            }
        };
    }

    // Transient trouble retries briefly (the host allows 30 s for connect);
    // what's left is either dead credentials (permanent) or an unreachable
    // homeserver (fatal — exit so the restart budget applies).
    let mut last_err: Option<matrix_sdk::Error> = None;
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        // timeout ZERO: this one-time sync's only job is recording
        // next_batch (the history discard) — never wait for new traffic.
        match client
            .sync_once(SyncSettings::new().timeout(Duration::ZERO))
            .await
        {
            Ok(response) => {
                // From here on, restarts recover instead of discarding.
                if let Err(err) = std::fs::write(initial_sync_marker(state_dir), b"") {
                    tracing::warn!("cannot write the initial-sync marker: {err}");
                }
                tracing::info!(
                    "first connect: pre-setup history discarded; live from token {}",
                    response.next_batch
                );
                return Ok((client, Some(response.next_batch)));
            }
            Err(err) if is_invalid_token(&err) => {
                return Err(ConnectError::Permanent(format!(
                    "homeserver rejected the stored access token (run `reset` then `setup`): {err}"
                )));
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(ConnectError::Fatal(format!(
        "initial sync failed: {}",
        last_err.expect("at least one attempt")
    )))
}

fn classify_build_error(err: ClientBuildError) -> ConnectError {
    match &err {
        ClientBuildError::MissingHomeserver
        | ClientBuildError::InvalidServerName
        | ClientBuildError::Url(_) => ConnectError::Permanent(format!(
            "invalid homeserver configuration (run `setup` again): {err}"
        )),
        _ => ConnectError::Fatal(format!("cannot build the matrix client: {err}")),
    }
}

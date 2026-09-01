//! E2EE support: the setup-time verification story (recovery keys, SAS) and
//! the unable-to-decrypt warn limiter (PLAN.md §3 "E2EE").
//!
//! Encrypted send/receive itself is transparent — the SDK encrypts outbound
//! events in encrypted rooms and decrypts inbound ones before our message
//! handler ever sees them. What lives here is everything around that:
//! making the device *verified* so peers trust it and history restores.
//!
//! The interactive flows print to the tty — they are `setup`-verb code that
//! happens to live next to the Matrix layer they drive.

use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use futures_util::StreamExt;
use matrix_sdk::encryption::verification::{
    SasState, SasVerification, Verification, VerificationRequest, VerificationRequestState,
};
use matrix_sdk::encryption::VerificationState;
use matrix_sdk::ruma::api::client::uiaa::{
    AuthData, MatrixUserIdentifier, Password, UserIdentifier,
};
use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
use matrix_sdk::Client;

/// At most one warn per window — a burst of undecryptable events (a whole
/// missed conversation) must not flood the operator.
pub struct BurstLimiter {
    window: Duration,
    last: Option<Instant>,
}

impl BurstLimiter {
    pub fn new(window: Duration) -> Self {
        BurstLimiter { window, last: None }
    }

    pub fn allow(&mut self, now: Instant) -> bool {
        let allowed = self
            .last
            .is_none_or(|last| now.duration_since(last) >= self.window);
        if allowed {
            self.last = Some(now);
        }
        allowed
    }
}

/// Bootstrap cross-signing (answering the UIAA challenge with the login
/// password) and enable recovery + key backup. Returns the recovery key —
/// caller shows it ONCE.
pub async fn bootstrap_and_enable_recovery(
    client: &Client,
    user_id: &str,
    password: &str,
) -> anyhow::Result<String> {
    let encryption = client.encryption();
    encryption.wait_for_e2ee_initialization_tasks().await;

    if let Err(err) = encryption.bootstrap_cross_signing_if_needed(None).await {
        let Some(uiaa) = err.as_uiaa_response() else {
            return Err(err).context("cross-signing bootstrap failed");
        };
        let mut auth = Password::new(
            UserIdentifier::Matrix(MatrixUserIdentifier::new(user_id.to_owned())),
            password.to_owned(),
        );
        auth.session = uiaa.session.clone();
        encryption
            .bootstrap_cross_signing(Some(AuthData::Password(auth)))
            .await
            .context("cross-signing bootstrap rejected by the homeserver")?;
    }

    let key = encryption
        .recovery()
        .enable()
        .wait_for_backups_to_upload()
        .await
        .context(
            "enabling recovery failed (a key backup may already exist — \
             choose the restore option instead)",
        )?;
    Ok(key)
}

/// Restore cross-signing + backup access from an existing recovery key.
pub async fn recover_with_key(client: &Client, recovery_key: &str) -> anyhow::Result<()> {
    let encryption = client.encryption();
    encryption.wait_for_e2ee_initialization_tasks().await;
    encryption
        .recovery()
        .recover(recovery_key)
        .await
        .context("recovery key rejected")?;
    Ok(())
}

/// Current verification verdict, as a status word.
pub async fn verification_summary(client: &Client) -> &'static str {
    client
        .encryption()
        .wait_for_e2ee_initialization_tasks()
        .await;
    match client.encryption().verification_state().get() {
        VerificationState::Verified => "verified",
        VerificationState::Unverified => "unverified",
        VerificationState::Unknown => "unknown",
    }
}

/// Wait for an emoji (SAS) verification started from another client logged
/// into the same account, and drive it on the tty. Requires sync: requests
/// only arrive through `/sync`, so this spawns one for the duration.
/// `Ok(true)` = verified.
pub async fn await_sas_verification(client: &Client, wait: Duration) -> anyhow::Result<bool> {
    let (request_tx, mut request_rx) = tokio::sync::mpsc::channel::<VerificationRequest>(1);
    let handle = client.add_event_handler({
        move |ev: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let request_tx = request_tx.clone();
            async move {
                if let Some(request) = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id)
                    .await
                {
                    let _ = request_tx.send(request).await;
                }
            }
        }
    });
    let sync_task = tokio::spawn({
        let client = client.clone();
        async move {
            let _ = client.sync(matrix_sdk::config::SyncSettings::new()).await;
        }
    });

    println!(
        "waiting up to {}s — start emoji verification from another client logged\n\
         into this account (this device shows up as \"terva\"), or Ctrl-C to stop…",
        wait.as_secs()
    );
    let request = tokio::time::timeout(wait, request_rx.recv()).await;
    client.remove_event_handler(handle);
    let outcome = match request {
        Ok(Some(request)) => drive_sas(request).await,
        _ => {
            println!("no verification request arrived — skipping");
            Ok(false)
        }
    };
    sync_task.abort();
    outcome
}

async fn drive_sas(request: VerificationRequest) -> anyhow::Result<bool> {
    request.accept().await.context("accepting the request")?;

    // The initiating client starts SAS once both sides are ready.
    let mut changes = request.changes();
    let sas: SasVerification = loop {
        let state = tokio::time::timeout(Duration::from_secs(120), changes.next())
            .await
            .context("timed out waiting for the other client to start SAS")?
            .context("verification stream ended")?;
        match state {
            VerificationRequestState::Transitioned {
                verification: Verification::SasV1(sas),
            } => break sas,
            VerificationRequestState::Cancelled(info) => {
                bail!("verification cancelled: {}", info.reason());
            }
            VerificationRequestState::Done => return Ok(true),
            _ => {}
        }
    };
    sas.accept().await.context("accepting SAS")?;

    let mut states = sas.changes();
    loop {
        let state = tokio::time::timeout(Duration::from_secs(180), states.next())
            .await
            .context("timed out during SAS")?
            .context("SAS stream ended")?;
        match state {
            SasState::KeysExchanged { emojis, .. } => {
                let Some(auth_string) = emojis else {
                    sas.cancel().await.ok();
                    bail!("the other client offered no emoji SAS");
                };
                println!("\ncompare with the other client:\n");
                for emoji in auth_string.emojis {
                    println!("  {}  {}", emoji.symbol, emoji.description);
                }
                if prompt_yes_no("\ndo the emojis match on both devices? [y/N]: ")? {
                    sas.confirm().await.context("confirming SAS")?;
                } else {
                    sas.mismatch().await.ok();
                    bail!("emoji mismatch — verification aborted");
                }
            }
            SasState::Done { .. } => {
                println!("device verified");
                return Ok(true);
            }
            SasState::Cancelled(info) => bail!("verification cancelled: {}", info.reason()),
            _ => {}
        }
    }
}

fn prompt_yes_no(prompt: &str) -> io::Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_limiter_allows_once_per_window() {
        let mut limiter = BurstLimiter::new(Duration::from_secs(60));
        let start = Instant::now();
        assert!(limiter.allow(start), "first event warns");
        assert!(
            !limiter.allow(start + Duration::from_secs(1)),
            "burst is quiet"
        );
        assert!(!limiter.allow(start + Duration::from_secs(59)));
        assert!(
            limiter.allow(start + Duration::from_secs(60)),
            "next window warns"
        );
        assert!(!limiter.allow(start + Duration::from_secs(61)));
    }
}

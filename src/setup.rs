//! The non-`run` lifecycle verbs: setup / status / reset / configured.
//!
//! House conventions (mirrors terva's reference connectors): setup and reset
//! run interactively on the inherited tty, prompting on stdout; status
//! prints one masked config block to stdout inside a 5 s host budget;
//! configured is a silent local-file predicate (exit 0 = configured).

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;

use crate::config::{
    self, config_path, load_config, load_session, save_config, session_path, store_path, Config,
};
use crate::matrix::client::build_client;

pub fn setup() -> ExitCode {
    match run_setup() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("matrix: setup: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_setup() -> anyhow::Result<()> {
    let state_dir = config::state_dir();
    let mut cfg = load_config(&state_dir)?;

    let homeserver = prompt_with_default(
        "homeserver URL (e.g. https://matrix.example.org)",
        &cfg.homeserver_url,
    )?;
    if homeserver.is_empty() {
        anyhow::bail!("no homeserver URL provided");
    }
    let user = prompt_with_default(
        "user id (e.g. @bot:example.org or a localpart)",
        &cfg.user_id,
    )?;
    if user.is_empty() {
        anyhow::bail!("no user id provided");
    }
    let password = prompt_password()?;
    if password.is_empty() {
        anyhow::bail!("no password provided");
    }

    // A stale session means a stale crypto store; a fresh login must not
    // inherit another device's Olm account. The old device stays live on
    // the server — `reset` first is the clean path and we say so. The
    // session may live in config.json (0.13+) or a legacy session.json.
    if load_session(&state_dir)?.is_some() || store_path(&state_dir).exists() {
        println!("replacing the existing session (the old device stays registered server-side; run `reset` first next time to log it out)");
        let _ = fs::remove_file(session_path(&state_dir));
        let _ = fs::remove_dir_all(store_path(&state_dir));
        let _ = fs::remove_file(crate::matrix::client::initial_sync_marker(&state_dir));
    }

    let runtime = tokio::runtime::Runtime::new()?;
    let client = runtime.block_on(build_client(&homeserver, Some(&store_path(&state_dir))))?;
    let (session, user_id, device_id) = runtime.block_on(async {
        let response = client
            .matrix_auth()
            .login_username(&user, &password)
            .initial_device_display_name("terva")
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("login rejected by the homeserver: {err}"))?;
        let session = matrix_sdk::authentication::matrix::MatrixSession::from(&response);
        anyhow::Ok((session, response.user_id, response.device_id))
    })?;

    // Persist the login before anything E2EE — a verification hiccup must
    // never cost the session. One write: the session rides inside
    // config.json now, sealed when the host has a recipient.
    cfg = Config {
        homeserver_url: homeserver,
        user_id: user_id.to_string(),
        device_id: device_id.to_string(),
        session: Some(session),
        ..cfg
    };
    save_config(&state_dir, &cfg)?;

    cfg.e2ee = e2ee_setup(&runtime, &client, user_id.as_str(), &password);
    cfg.e2ee_source = "setup".into();
    save_config(&state_dir, &cfg)?;

    // The sqlite pool aborts if destroyed outside a runtime context.
    runtime.block_on(async move { drop(client) });

    println!(
        "\nsaved: {user_id} (device {device_id}) to {}",
        config_path(&state_dir).display()
    );
    println!("next: run `terva bot run --connector matrix`, then invite your bot to a DM.");
    Ok(())
}

/// The E2EE verification story (PLAN.md §3 "E2EE"). Best-effort by design:
/// failures warn and the summary says so — the bot still works in encrypted
/// rooms going forward, just unverified.
fn e2ee_setup(
    runtime: &tokio::runtime::Runtime,
    client: &matrix_sdk::Client,
    user_id: &str,
    password: &str,
) -> String {
    use crate::matrix::e2ee;

    println!("\ne2ee — how should this device get verified?");
    println!("  [1] create a new recovery key (fresh bot account; the key prints once)");
    println!("  [2] restore from an existing recovery key");
    println!("  [3] skip (encrypted rooms still work going forward; verify later)");
    let choice = prompt_with_default("choice", "1").unwrap_or_else(|_| "3".into());

    let recovery = match choice.as_str() {
        "2" => {
            let key = prompt_secret("recovery key (input hidden): ").unwrap_or_default();
            if key.is_empty() {
                eprintln!("matrix: e2ee: no recovery key provided — skipping");
                "recovery skipped"
            } else {
                match runtime.block_on(e2ee::recover_with_key(client, key.trim())) {
                    Ok(()) => "recovery restored",
                    Err(err) => {
                        eprintln!("matrix: e2ee: {err:#}");
                        "recovery failed"
                    }
                }
            }
        }
        "3" => "recovery skipped",
        _ => match runtime.block_on(e2ee::bootstrap_and_enable_recovery(
            client, user_id, password,
        )) {
            Ok(key) => {
                println!(
                    "\nrecovery key (shown ONCE — store it safely; a future `setup` can restore with it):\n\n  {key}\n"
                );
                "recovery enabled"
            }
            Err(err) => {
                eprintln!("matrix: e2ee: {err:#}");
                "recovery failed"
            }
        },
    };

    let mut verdict = runtime.block_on(e2ee::verification_summary(client));
    if verdict != "verified" {
        let verify = prompt_with_default("verify this device from another client now? (y/N)", "n")
            .unwrap_or_else(|_| "n".into());
        if matches!(verify.as_str(), "y" | "Y" | "yes") {
            match runtime.block_on(e2ee::await_sas_verification(
                client,
                std::time::Duration::from_secs(120),
            )) {
                Ok(_) => {}
                Err(err) => eprintln!("matrix: e2ee: verification: {err:#}"),
            }
            verdict = runtime.block_on(e2ee::verification_summary(client));
        }
    }
    format!("{recovery}, device {verdict}")
}

pub fn status() -> ExitCode {
    match run_status() {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("matrix: status: {err}");
            ExitCode::FAILURE
        }
    }
}

/// One masked config block — pasteable into bug reports without leaking
/// secrets. Local file reads only (the host gives status 5 s).
fn run_status() -> anyhow::Result<String> {
    let state_dir = config::state_dir();
    let cfg = load_config(&state_dir)?;
    let Some(session) = load_session(&state_dir)? else {
        return Ok("not configured (run `terva bot setup --connector matrix`)".into());
    };
    let store = if store_path(&state_dir).is_dir() {
        "present"
    } else {
        "missing"
    };
    // An empty field is not a verdict of "unverified" — it is the absence
    // of one, which is what a setup killed before its final save leaves
    // behind. Saying so points at the verb that fixes it instead of
    // implying the device failed verification.
    let e2ee = if cfg.e2ee.is_empty() {
        "not recorded (run the `verify` verb)".to_string()
    } else if cfg.e2ee_source == "verify" {
        format!("{} (as of the last verify)", cfg.e2ee)
    } else {
        format!("{} (as of setup)", cfg.e2ee)
    };
    let speaker = if cfg.speaker.is_empty() {
        "off"
    } else {
        cfg.speaker.as_str()
    };
    Ok(format!(
        "matrix bot:   {} (device {})\n\
         homeserver:   {}\n\
         token:        {}\n\
         auto join:    {}\n\
         e2ee:         {e2ee}\n\
         speaker:      {speaker}\n\
         store:        {store}\n\
         config file:  {}",
        session.meta.user_id,
        session.meta.device_id,
        cfg.homeserver_url,
        mask_token(&session.tokens.access_token),
        cfg.auto_join,
        config_path(&state_dir).display(),
    ))
}

pub fn verify() -> ExitCode {
    match run_verify() {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("matrix: verify: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Read the live verification verdict, record it, and optionally wait for
/// SAS — all on the session we already have.
///
/// `setup` writes its verdict only after its interactive tail returns, so a
/// setup killed during recovery or the SAS wait leaves a working session
/// and no verdict at all. The only other verb that touches E2EE is `setup`
/// itself, and it logs in unconditionally: answering a read-only question
/// with it costs a second registered device. This restores the saved
/// session against the existing store instead, so it adds no device and
/// creates no recovery key.
fn run_verify() -> anyhow::Result<String> {
    use crate::matrix::e2ee;

    let state_dir = config::state_dir();
    let mut cfg = load_config(&state_dir)?;
    let Some(session) = load_session(&state_dir)? else {
        anyhow::bail!("not configured (run `terva bot setup --connector matrix`)");
    };
    if cfg.homeserver_url.is_empty() {
        anyhow::bail!("no homeserver URL in the config");
    }

    let runtime = tokio::runtime::Runtime::new()?;
    let client = runtime.block_on(build_client(
        &cfg.homeserver_url,
        Some(&store_path(&state_dir)),
    ))?;
    runtime
        .block_on(client.restore_session(session))
        .context("restoring the saved session")?;

    let mut verdict = runtime.block_on(e2ee::observe_verdict(&client, Duration::from_secs(30)))?;
    println!("device {}: {verdict}", cfg.device_id);
    // "unverified" has two causes that want opposite fixes, and the word
    // alone hides which one this is. Either the server has no signature on
    // this device, or we hold no cross-signing identity to check a
    // signature against — the second is a recovery problem, and no amount
    // of emoji will move it.
    if verdict != "verified" && !runtime.block_on(e2ee::own_identity_known(&client)) {
        println!(
            "note: this store holds no cross-signing identity for {}, so no device here can read as verified.\n\
             \x20     Restore it with a recovery key (`setup`, option 2) rather than another emoji round.",
            cfg.user_id
        );
    }
    // Save before the interactive tail, not after it. Losing the verdict to
    // a closed terminal is the whole reason this verb exists, and the SAS
    // wait below is exactly where that happened last time.
    record_verdict(&state_dir, &mut cfg, verdict)?;

    if verdict != "verified" {
        let answer = prompt_with_default(
            "wait for emoji verification from another client now? (y/N)",
            "n",
        )
        .unwrap_or_else(|_| "n".into());
        if matches!(answer.as_str(), "y" | "Y" | "yes") {
            match runtime.block_on(e2ee::await_sas_verification(
                &client,
                Duration::from_secs(120),
            )) {
                Ok(_) => {}
                Err(err) => eprintln!("matrix: e2ee: verification: {err:#}"),
            }
            verdict = runtime.block_on(e2ee::observe_verdict(&client, Duration::from_secs(30)))?;
            record_verdict(&state_dir, &mut cfg, verdict)?;
        }
    }

    // The sqlite pool aborts if destroyed outside a runtime context.
    runtime.block_on(async move { drop(client) });

    Ok(match verdict {
        "verified" => format!("device {} is verified — recorded", cfg.device_id),
        "unverified" => format!(
            "device {} is NOT verified — recorded. Verify it from another client, then run this again.",
            cfg.device_id
        ),
        _ => format!(
            "device {} is not in the crypto store — recorded. The store may predate this session; `reset` then `setup` rebuilds it.",
            cfg.device_id
        ),
    })
}

/// One write per reading, so an interrupted verify still leaves the last
/// thing we actually knew.
fn record_verdict(state_dir: &Path, cfg: &mut Config, verdict: &str) -> anyhow::Result<()> {
    cfg.e2ee = format!("device {verdict}");
    cfg.e2ee_source = "verify".into();
    save_config(state_dir, cfg)?;
    Ok(())
}

pub fn configured() -> ExitCode {
    let state_dir = config::state_dir();
    match load_session(&state_dir) {
        Ok(Some(session)) if !session.tokens.access_token.is_empty() => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

pub fn reset() -> ExitCode {
    match run_reset() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("matrix: reset: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Invalidate the device server-side (a deleted-but-live token is a leak),
/// then wipe our files. NEVER touches `data/` (host staging) or
/// `pairing.json` (host-owned).
fn run_reset() -> anyhow::Result<()> {
    let state_dir = config::state_dir();
    let cfg = load_config(&state_dir)?;
    if let Some(session) = load_session(&state_dir)? {
        if !cfg.homeserver_url.is_empty() {
            let runtime = tokio::runtime::Runtime::new()?;
            let logout = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    // In-memory store is fine for a single logout call.
                    let client = build_client(&cfg.homeserver_url, None).await?;
                    client.restore_session(session).await?;
                    client.logout().await?;
                    anyhow::Ok(())
                })
                .await
                .map_err(|_| anyhow::anyhow!("timed out"))?
            });
            match logout {
                Ok(()) => println!("logged out the device server-side"),
                Err(err) => eprintln!(
                    "matrix: server-side logout failed (the token may stay valid until it expires): {err}"
                ),
            }
        }
    }

    let mut removed = false;
    for path in [
        session_path(&state_dir),
        config_path(&state_dir),
        // The sync-resume marker must die with the store: a fresh setup's
        // first connect must discard history again, not resume a token
        // the wiped store no longer holds.
        crate::matrix::client::initial_sync_marker(&state_dir),
    ] {
        removed |= remove_reported(&path, fs::remove_file(&path));
    }
    let store = store_path(&state_dir);
    removed |= remove_reported(&store, fs::remove_dir_all(&store));
    if !removed {
        println!("no matrix config to remove");
    }
    Ok(())
}

fn remove_reported(path: &Path, result: io::Result<()>) -> bool {
    match result {
        Ok(()) => {
            println!("removed {}", path.display());
            true
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => false,
        Err(err) => {
            eprintln!("matrix: cannot remove {}: {err}", path.display());
            false
        }
    }
}

/// Hidden password input on a tty; a plain stdin line otherwise, so headless
/// provisioning can pipe all three setup answers:
/// `printf 'https://hs\nuser\npass\n' | … setup`.
fn prompt_password() -> io::Result<String> {
    prompt_secret("password (input hidden): ")
}

fn prompt_secret(prompt: &str) -> io::Result<String> {
    use std::io::IsTerminal;
    if io::stdin().is_terminal() {
        rpassword::prompt_password(prompt)
    } else {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }
}

/// Prompt on stdout (house style: trailing ": ", read one trimmed line).
/// A non-empty default is shown in brackets and kept on empty input.
fn prompt_with_default(label: &str, default: &str) -> io::Result<String> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let value = line.trim();
    Ok(if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    })
}

/// Keep enough of the token to correlate, never enough to use.
fn mask_token(token: &str) -> String {
    match (token.get(..4), token.get(token.len().saturating_sub(4)..)) {
        (Some(head), Some(tail)) if token.len() > 10 => format!("{head}...{tail}"),
        _ => "<hidden>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::mask_token;

    #[test]
    fn masking_never_leaks_short_tokens() {
        assert_eq!(mask_token(""), "<hidden>");
        assert_eq!(mask_token("shorttoken"), "<hidden>");
        assert_eq!(mask_token("syt_abcdefghijklmnop_XYZ1"), "syt_...XYZ1");
    }
}

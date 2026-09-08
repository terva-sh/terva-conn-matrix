//! State-dir layout and config/session persistence.
//!
//! `$TERVA_HOME/connectors/matrix/` is ours (mirrors `connsdk.StateDir`):
//!
//! ```text
//! config.json      — homeserver_url, user_id, device_id, auto_join,
//!                    max_attachment_mb, and (since 0.13.0) the matrix-sdk
//!                    session bundle under "session", its token values
//!                    SEALED in place (enc:age:v2, dual-recipient) when the
//!                    host has at-rest encryption configured
//! secrets.key      — this connector's own age identity (SDK-minted on the
//!                    first sealed save; on the host's read deny-list)
//! session.json     — pre-0.13 location of the session bundle; migrated
//!                    into config.json and removed (migrate_legacy_session)
//! store/           — matrix-sdk sqlite state + crypto stores
//! data/            — the HOST-assigned attachment staging dir (host sweeps it)
//! pairing.json     — HOST-owned; never read or write
//! ```
//!
//! Files are written atomically (tmp + rename), dirs 0700, secrets 0600 —
//! the house `privfs` discipline. The sealing itself is
//! `terva_connsdk::SealedState` (P9): every path in [`SECRET_PATHS`] is
//! sealed to the connector's own key AND terva's recipient, everything
//! else stays plaintext and inspectable. The same paths are declared in
//! the hello, which is what earns this directory its agent-readability
//! under the host's per-read gate — a declaration that lied by omission
//! (a token outside the declared file) would defeat that gate, which is
//! why the session bundle had to move INTO config.json.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use matrix_sdk::authentication::matrix::MatrixSession;
use serde::{Deserialize, Serialize};
use terva_connsdk::SealedState;
use terva_env::connector_state_dir;

pub const CONNECTOR_NAME: &str = "matrix";

/// The JSON Pointers in config.json that hold secret material — the sealed
/// set, and the hello declaration. `refresh_token` is declared even though
/// this connector does not refresh today: declaring a path that is absent
/// costs nothing, while acquiring a refresh token later under an undeclared
/// path would silently leave it plaintext.
pub const SECRET_PATHS: [&str; 2] = ["/session/access_token", "/session/refresh_token"];

/// Connector configuration, created by `setup`, read by every verb.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub homeserver_url: String,
    pub user_id: String,
    pub device_id: String,
    /// "always" (default) or "never" — whether to accept room invites.
    /// Config-gated admission policy stays host-side; this only bounds what
    /// the bot's account does at the Matrix layer.
    pub auto_join: String,
    /// Inbound attachment size ceiling (enforced from phase 5 on).
    pub max_attachment_mb: u64,
    /// E2EE provisioning summary written by `setup` or `verify` (e.g.
    /// "recovery enabled, device verified"), shown by `status` — a
    /// snapshot, so `status` never has to open the live crypto store.
    ///
    /// Empty means no verdict was ever recorded, which is NOT the same as
    /// "unverified". `setup` writes this only after its interactive tail
    /// returns, so a setup killed during recovery or the SAS wait leaves a
    /// working session with this field empty. `verify` exists to fill it in
    /// without logging in a new device.
    pub e2ee: String,
    /// Which verb last wrote [`Self::e2ee`]: "verify" for a live read of
    /// the crypto store, anything else for a setup-time note. Empty is the
    /// shape of every config written before this field existed, and back
    /// then only `setup` wrote a verdict at all. `status` says which,
    /// because a snapshot from provisioning and a reading taken just now
    /// age very differently.
    pub e2ee_source: String,
    /// MSC4144 per-message speaker profiles: "" / "off" (default, the host
    /// renders its `**Name:**` prefix fallback), "name_only", or "full"
    /// (avatars uploaded too). Off until per-message-profile rendering is
    /// common in clients; edit config.json to opt in.
    pub speaker: String,
    /// The matrix-sdk session bundle (access token). Inside config.json —
    /// not a sibling file — because [`SECRET_PATHS`] can only vouch for
    /// values in the declared state file; a token beside it would sit in a
    /// directory the host's gate had been told was clean.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<MatrixSession>,
}

impl Config {
    pub fn auto_join_enabled(&self) -> bool {
        self.auto_join != "never"
    }

    /// The declared speaker grade, when the flag opts in.
    pub fn speaker_feature(&self) -> Option<&'static str> {
        match self.speaker.as_str() {
            "name_only" => Some("speaker:name_only"),
            "full" => Some("speaker:full"),
            _ => None,
        }
    }
}

/// The connector state dir: `<terva home>/connectors/matrix` — home and
/// layout resolution live in terva-env (the `envcompat.Home()` analog).
pub fn state_dir() -> PathBuf {
    connector_state_dir(CONNECTOR_NAME)
}

pub fn config_path(state_dir: &Path) -> PathBuf {
    state_dir.join("config.json")
}

pub fn session_path(state_dir: &Path) -> PathBuf {
    state_dir.join("session.json")
}

pub fn store_path(state_dir: &Path) -> PathBuf {
    state_dir.join("store")
}

/// The SealedState for this connector, rooted at the home `state_dir` sits
/// in — `state_dir` IS `<home>/connectors/matrix` (the module-doc layout),
/// so the home is two levels up. Deriving it keeps every caller's explicit
/// state-dir threading working, tests included, with no env mutation.
pub fn sealed_state(state_dir: &Path) -> SealedState {
    let home = state_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or(state_dir);
    SealedState::with_home(home, CONNECTOR_NAME, SECRET_PATHS)
}

/// Missing file = default config, not an error (house convention — and
/// `#[serde(default)]` on [`Config`] is what absorbs the empty document the
/// SDK hands back). Declared secret values come back opened (the
/// connector's own key); with no key yet nothing we wrote is sealed and the
/// bytes pass through.
///
/// Through `load_as` rather than serde directly: its error carries a
/// position but not serde's message, which would quote the value it choked
/// on — and by this point that value is an OPENED access token.
pub fn load_config(state_dir: &Path) -> io::Result<Config> {
    let mut config: Config = sealed_state(state_dir).load_as()?;
    if config.auto_join.is_empty() {
        config.auto_join = "always".into();
    }
    if config.max_attachment_mb == 0 {
        config.max_attachment_mb = 64;
    }
    Ok(config)
}

/// Seals [`SECRET_PATHS`] and writes owner-only. With no terva recipient
/// configured (a host that never ran `terva secret init`) the file is
/// written plaintext, exactly as before 0.13.0, converting on the first
/// save after encryption turns on.
pub fn save_config(state_dir: &Path, config: &Config) -> io::Result<()> {
    sealed_state(state_dir).save_as(config)
}

/// `Ok(None)` when no session has been provisioned. Since 0.13.0 the
/// session lives inside config.json; a pre-0.13 `session.json` is still
/// honored until [`migrate_legacy_session`] sweeps it in.
pub fn load_session(state_dir: &Path) -> io::Result<Option<MatrixSession>> {
    if let Some(session) = load_config(state_dir)?.session {
        return Ok(Some(session));
    }
    match fs::read(session_path(state_dir)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).map_err(io::Error::other)?,
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Sweeps a pre-0.13 `session.json` into the sealed config.json and removes
/// it. Idempotent; a no-op when no legacy file exists. This runs before the
/// hello declares [`SECRET_PATHS`]: the declaration is what the host's
/// per-read gate trusts, and it must not lie by omission — a plaintext
/// token in an undeclared sibling file is exactly such a lie.
pub fn migrate_legacy_session(state_dir: &Path) -> io::Result<()> {
    let legacy = session_path(state_dir);
    let bytes = match fs::read(&legacy) {
        Ok(b) => b,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let session: MatrixSession = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let mut config = load_config(state_dir)?;
    if config.session.is_none() {
        config.session = Some(session);
        save_config(state_dir, &config)?;
    }
    // A session already inside config.json outranks the legacy file: the
    // merged model is the one every writer has used since 0.13.0.
    fs::remove_file(&legacy)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home-shaped scratch tree: the state dir is `<home>/connectors/matrix`
    /// (the layout `sealed_state` derives the home back out of). Returns the
    /// state dir; its grandparent is the home.
    fn temp_state_dir(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("terva-conn-matrix-test-{name}"));
        let _ = fs::remove_dir_all(&home);
        let dir = home.join("connectors").join(CONNECTOR_NAME);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_home_of(state_dir: &Path) -> PathBuf {
        state_dir.parent().unwrap().parent().unwrap().to_path_buf()
    }

    fn cleanup(state_dir: &Path) {
        let _ = fs::remove_dir_all(temp_home_of(state_dir));
    }

    fn test_session() -> MatrixSession {
        // The flattened MatrixSession JSON shape the SDK expects.
        serde_json::from_value(serde_json::json!({
            "user_id": "@bot:example.org",
            "device_id": "ABCDEFG",
            "access_token": "syt_secret_token",
        }))
        .unwrap()
    }

    #[test]
    fn config_roundtrip_and_defaults() {
        let dir = temp_state_dir("config");
        assert!(load_config(&dir).unwrap().auto_join_enabled());
        let config = Config {
            homeserver_url: "https://matrix.example.org".into(),
            user_id: "@bot:example.org".into(),
            device_id: "ABCDEFG".into(),
            auto_join: "always".into(),
            max_attachment_mb: 64,
            ..Default::default()
        };
        save_config(&dir, &config).unwrap();
        let loaded = load_config(&dir).unwrap();
        assert_eq!(loaded.homeserver_url, config.homeserver_url);
        assert_eq!(loaded.device_id, "ABCDEFG");
        cleanup(&dir);
    }

    #[test]
    fn session_roundtrip_inside_config() {
        let dir = temp_state_dir("session");
        assert!(load_session(&dir).unwrap().is_none());
        let session = test_session();
        let config = Config {
            session: Some(session.clone()),
            ..Default::default()
        };
        save_config(&dir, &config).unwrap();
        let loaded = load_session(&dir).unwrap().expect("session");
        assert_eq!(loaded, session);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(config_path(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "config.json holds the token now");
        }
        // No terva recipient in this scratch home: plaintext degradation,
        // and no connector key minted.
        assert!(fs::read_to_string(config_path(&dir))
            .unwrap()
            .contains("syt_secret_token"));
        assert!(!sealed_state(&dir).key_path().exists());
        cleanup(&dir);
    }

    #[test]
    fn sealing_engages_when_the_host_has_a_recipient() {
        let dir = temp_state_dir("sealing");
        let home = temp_home_of(&dir);
        // A host that ran `terva secret init`: its public recipient sits in
        // config.json. Use a real identity so the value provably opens.
        let terva_id = age::x25519::Identity::generate();
        fs::write(
            home.join("config.json"),
            format!(
                "{{\"secrets\":{{\"recipient\":\"{}\"}}}}\n",
                terva_id.to_public()
            ),
        )
        .unwrap();

        let config = Config {
            homeserver_url: "https://matrix.example.org".into(),
            session: Some(test_session()),
            ..Default::default()
        };
        save_config(&dir, &config).unwrap();

        let on_disk = fs::read_to_string(config_path(&dir)).unwrap();
        assert!(
            !on_disk.contains("syt_secret_token"),
            "the token must be sealed on disk:\n{on_disk}"
        );
        assert!(on_disk.contains("enc:age:v2:"));
        assert!(
            on_disk.contains("https://matrix.example.org"),
            "non-secret values stay plaintext"
        );
        let state = sealed_state(&dir);
        assert!(state.key_path().exists(), "own key minted on sealed save");
        state.clean().expect("every declared path sealed");
        assert!(
            state.recipient().unwrap().is_some(),
            "the hello declaration has a recipient to carry"
        );

        // And the opened view is unchanged for every reader.
        let session = load_session(&dir).unwrap().expect("session");
        assert_eq!(session.tokens.access_token, "syt_secret_token");
        cleanup(&dir);
    }

    #[test]
    fn legacy_session_json_is_migrated_in_and_removed() {
        let dir = temp_state_dir("migrate");
        // A 0.12-shaped tree: config.json without a session, session.json
        // beside it holding the token.
        save_config(
            &dir,
            &Config {
                homeserver_url: "https://matrix.example.org".into(),
                ..Default::default()
            },
        )
        .unwrap();
        fs::write(
            session_path(&dir),
            serde_json::to_vec(&test_session()).unwrap(),
        )
        .unwrap();

        // Readable through the fallback before migration...
        assert!(load_session(&dir).unwrap().is_some());
        migrate_legacy_session(&dir).unwrap();
        // ...and through config.json after, with the legacy file gone.
        assert!(!session_path(&dir).exists());
        let config = load_config(&dir).unwrap();
        assert_eq!(
            config.session.expect("merged").tokens.access_token,
            "syt_secret_token"
        );
        assert_eq!(config.homeserver_url, "https://matrix.example.org");

        // Idempotent.
        migrate_legacy_session(&dir).unwrap();
        cleanup(&dir);
    }
}

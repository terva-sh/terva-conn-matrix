//! House-convention guards (see terva-sh/docs extensions/conventions.md):
//! version lockstep across Cargo.toml and connector.json, and the launcher's
//! exec bit (git tracks it; a plain file write drops it).

use std::path::Path;

#[test]
fn manifest_version_matches_cargo() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("connector.json"))
            .expect("read connector.json");
    let manifest: serde_json::Value =
        serde_json::from_str(&manifest).expect("connector.json is JSON");
    assert_eq!(
        manifest["version"].as_str().expect("version"),
        env!("CARGO_PKG_VERSION"),
        "connector.json version and Cargo.toml version must be bumped together"
    );
    assert_eq!(manifest["name"], "matrix");
    assert_eq!(manifest["exec"], "./run.sh");
}

#[cfg(unix)]
#[test]
fn run_sh_is_executable() {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(Path::new(env!("CARGO_MANIFEST_DIR")).join("run.sh"))
        .expect("stat run.sh")
        .permissions()
        .mode();
    assert!(
        mode & 0o111 != 0,
        "run.sh must keep its exec bit — terva execs it verbatim; a 644 launcher dies on EACCES with an empty log"
    );
}

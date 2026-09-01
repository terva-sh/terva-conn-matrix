# terva-conn-matrix dev tasks. Run `just` to list.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Maintainer-only release plumbing. Optional import: the public mirror
# omits release.just, and this justfile must keep working there.
import? "release.just"

# List recipes.
default:
    @just --list

# Build the release binary, the way run.sh does (--locked = pinned Cargo.lock).
build:
    cargo build --release --locked
    @echo "built target/release/terva-conn-matrix"

# Run the tests (proto goldens, framing, wire smoke, convention guards).
test *ARGS:
    cargo test --release {{ARGS}}

# Formatting + lint gate: rustfmt check and clippy (warnings are errors).
lint:
    cargo fmt --check
    cargo clippy --release --all-targets -- -D warnings

# Format all sources.
fmt:
    cargo fmt

# Pre-push gate: formatting, lint, and the full locked test suite.
ci: lint
    cargo test --release --locked

# Install the pre-push hook that runs `just ci`. The remote workflow
# (.github/workflows/ci.yml) needs a runner that can fetch the SDK pin;
# this needs nothing, and it is the gate 0.13.0 went out without — it
# shipped with a red `just ci` because only `cargo test` had been run.
# Bypass a single push with `git push --no-verify`.
hooks:
    #!/usr/bin/env bash
    set -euo pipefail
    hook="$(git rev-parse --git-path hooks/pre-push)"
    mkdir -p "$(dirname "$hook")"
    printf '%s\n' '#!/usr/bin/env bash' \
        '# Installed by `just hooks`. Bypass once with `git push --no-verify`.' \
        'exec just ci' > "$hook"
    chmod +x "$hook"
    echo "installed $hook"

# Start the throwaway local Synapse + preconfigured Element Web (docker).
synapse:
    mkdir -p testing/synapse/data
    docker compose -f testing/synapse/compose.yaml up -d --wait
    @echo "synapse up:  http://127.0.0.1:${SYNAPSE_PORT:-18008} (server_name: localhost, throwaway)"
    @echo "element up:  http://127.0.0.1:${ELEMENT_PORT:-18009} (already pointed at the throwaway)"

# Stop the throwaway Synapse (state kept; `just synapse-clean` wipes it).
synapse-down:
    docker compose -f testing/synapse/compose.yaml down

# Stop the throwaway Synapse and wipe its state — next start is factory-fresh.
synapse-clean: synapse-down
    rm -rf testing/synapse/data

# Live compliance suite against the throwaway Synapse (starts it if needed).
# Hermetic tests stay in `just test`; these are #[ignore]d without the env.
e2e: synapse
    TERVA_CONN_MATRIX_E2E_HS="http://127.0.0.1:${SYNAPSE_PORT:-18008}" \
        cargo test --release --test live_synapse -- --ignored --nocapture

# Symlink this checkout's manifest into terva (dev install; run.sh builds).
link:
    terva bot link "$(pwd)/connector.json"

# Build a release archive: prebuilt binary + manifest + launcher + docs,
# named terva-conn-matrix-<version>-<target-triple>.tar.gz under dist/.
# run.sh prefers the bundled binary, so the unpacked directory works with
# `terva bot link` on a machine with no Rust toolchain.
dist: build
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(just version)"
    target="$(rustc -vV | sed -n 's/^host: //p')"
    stage="dist/terva-conn-matrix"
    rm -rf dist && mkdir -p "$stage"
    cp target/release/terva-conn-matrix connector.json run.sh README.md CHANGELOG.md LICENSE "$stage/"
    tar -C dist -czf "dist/terva-conn-matrix-${version}-${target}.tar.gz" terva-conn-matrix
    echo "dist/terva-conn-matrix-${version}-${target}.tar.gz"

# Print the crate version (kept in lockstep by tests/conventions.rs).
version:
    @grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"(.*)".*/\1/'

# Remove build output.
clean:
    cargo clean

#!/usr/bin/env bash
# matrix connector launcher.
#
# terva runs a connector by executing the manifest's `exec` verbatim with the
# lifecycle verb (run/setup/status/reset/configured) appended as the LAST
# argument — it never compiles Rust. This wrapper builds the binary when the
# sources are newer, then execs it with the arguments untouched.
#
# IMPORTANT: stdout is the protocol wire during `run`. Every byte of build
# chatter goes to stderr (terva captures it to
# $TERVA_HOME/logs/connector-matrix.log); a stray stdout write corrupts the
# JSON frame stream.
set -euo pipefail
cd "$(dirname "$0")"

# A `just dist` archive ships the prebuilt binary right next to this
# launcher — prefer it, no toolchain needed.
if [ -x "./terva-conn-matrix" ]; then
	exec ./terva-conn-matrix "$@"
fi

bin="target/release/terva-conn-matrix"

needs_build() {
	[ -x "$bin" ] || return 0
	# Rebuild if any source or manifest is newer than the binary.
	if [ -n "$(find src tests -name '*.rs' -newer "$bin" -print -quit 2>/dev/null)" ]; then
		return 0
	fi
	if [ Cargo.toml -nt "$bin" ]; then
		return 0
	fi
	if [ -f Cargo.lock ] && [ Cargo.lock -nt "$bin" ]; then
		return 0
	fi
	return 1
}

if needs_build; then
	if ! command -v cargo >/dev/null 2>&1; then
		echo "[matrix] Rust/cargo not found — install via https://rustup.rs (needs rustc >= 1.93)." >&2
		exit 1
	fi
	echo "[matrix] building $bin (first launch or sources changed)…" >&2
	# --locked: build only from the audited, pinned Cargo.lock.
	cargo build --release --locked >&2
	echo "[matrix] build complete." >&2
fi

exec "$bin" "$@"

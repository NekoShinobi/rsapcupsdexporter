# rsapcupsdexporter — common repo workflows.
#
#   just          list every recipe
#   just dev      run the exporter locally, rebuilding on every change
#   just ci       everything that should pass before a commit
#
# There is no -api/-ui split here: this is a single exporter with no frontend,
# so the plain verbs are the whole thing.

set dotenv-load := true
set shell := ["bash", "-euo", "pipefail", "-c"]

# Host identity, so anything the dev container writes into the bind mount stays
# owned by you instead of root. Only `up-dev` needs these.
export DEV_UID := env_var_or_default("DEV_UID", `id -u`)
export DEV_GID := env_var_or_default("DEV_GID", `id -g`)

# Supply-chain cooldown: never adopt a release younger than this. Most malicious
# package releases are found and yanked within a few days, so waiting costs
# nothing and skips the window where you would be the one to find it.
# renovate.json's `minimumReleaseAge` MUST carry the same number — Renovate
# opens the automated PRs and cannot read this file.
DEPS_MIN_AGE_DAYS := "3"

# List available recipes.
[private]
default:
    @just --list

# ── Development ───────────────────────────────────────────────────────────────

# Fetch dependencies exactly as locked.
[group('dev')]
setup:
    cargo fetch --locked

# Run the exporter with live reload.
[group('dev')]
dev:
    bacon --headless run

# Run the exporter once, without a watcher.
[group('dev')]
run *args:
    cargo run -- {{ args }}

# ── Build ─────────────────────────────────────────────────────────────────────

# Build the optimized binary.
[group('build')]
build:
    cargo build --release

# ── Quality ───────────────────────────────────────────────────────────────────

# Fast type-check — no formatting, no linting, no tests.
[group('checks')]
check:
    cargo check --all-targets

# Run the test suite.
[group('checks')]
test:
    cargo test --locked --all-targets

# Clippy over all targets, warnings promoted to errors.
[group('checks')]
lint:
    cargo clippy --locked --all-targets -- -D warnings

# Format sources in place.
[group('checks')]
fmt:
    cargo fmt --all

# Verify formatting without changing files.
[group('checks')]
fmt-check:
    cargo fmt --all --check

# Everything that should pass before a commit; also what CI runs.
[group('checks')]
ci: fmt-check check lint test

# ── Dependencies ──────────────────────────────────────────────────────────────

# Show available dependency updates without changing anything.
[group('deps')]
deps-outdated:
    cargo update --dry-run

# Refresh Cargo.lock within the declared semver ranges, honouring the cooldown.
[group('deps')]
deps-update:
    #!/usr/bin/env bash
    set -euo pipefail
    days="{{ DEPS_MIN_AGE_DAYS }}"

    # -Zmin-publish-age is nightly-only, but this crate builds on stable and CI
    # pins stable — so nightly is requested for this one command rather than
    # pinned project-wide in rust-toolchain.toml.
    if ! cargo +nightly -Z help 2>&1 | grep -q 'min-publish-age'; then
      echo "error: nightly cargo has no -Zmin-publish-age, so the ${days}-day cooldown cannot be enforced." >&2
      echo "Install it with: rustup toolchain install nightly" >&2
      exit 1
    fi

    cargo +nightly update -Z min-publish-age --config "registry.global-min-publish-age = \"${days} days\""

# Scan dependencies for known vulnerabilities.
[group('deps')]
deps-audit:
    cargo audit

# Validate the Renovate policy that opens the automated update PRs.
[group('deps')]
deps-validate:
    bunx --package renovate renovate-config-validator --strict

# ── Docker ────────────────────────────────────────────────────────────────────

# Build the production image locally.
[group('docker')]
docker-build:
    docker build -t rsapcupsdexporter:local .

# Start the production stack in the foreground.
[group('docker')]
up:
    docker compose -f compose.yml up --build

# Start the production stack in the background.
[group('docker')]
up-detach:
    docker compose -f compose.yml up --build -d

# Start the dev stack with live reload (parity path — `just dev` is faster).
[group('docker')]
up-dev:
    docker compose -f compose.dev.yml up --build

# Stop every stack; neither failing should prevent the other from stopping.
[group('docker')]
down:
    -docker compose -f compose.dev.yml down
    -docker compose -f compose.yml down

# Follow production container logs.
[group('docker')]
logs:
    docker compose -f compose.yml logs -f

# Follow dev container logs.
[group('docker')]
logs-dev:
    docker compose -f compose.dev.yml logs -f

# Validate the Compose files without starting anything.
[group('docker')]
compose-check:
    docker compose -f compose.yml config --quiet
    docker compose -f compose.dev.yml config --quiet

#!/usr/bin/env bash
#
# cloud-setup.sh — one-shot environment bootstrap for Claude Code on the web.
#
# This script is pasted into the cloud environment's "Setup script" field and
# runs ONCE, as root, at environment-create time. Critically, it runs BEFORE
# the repo is cloned into the sandbox, so it must be fully self-contained: it
# may not read any file from this repository (rust-toolchain.toml, Brewfile,
# CI YAML, etc.). Every version it needs is therefore hard-coded below and kept
# in sync with those files by hand — see the "PINNED VERSIONS" block.
#
# Design rules:
#   * Idempotent. Re-running must be a no-op, not a re-install. Every step is
#     guarded by a check-before-install test so a second run does nothing.
#   * Best-effort for optional tooling. The build/test path (cargo xtask test)
#     is mandatory and its failures are fatal; docs (mdbook) and Docker warming
#     are conveniences and never abort the script.
#   * No repo dependency. Do not `cd` into or read from the checkout.
#
# Kept in sync with:
#   rust-toolchain.toml ....... PINNED_NIGHTLY, RUST_COMPONENTS, RUST_TARGETS
#   .github/workflows/build.yml  APT_PACKAGES (the "Install system dependencies" step)
#   my local ~/.claude setup .... SISYPHUS_PKG (oh-my-claude-sisyphus@1.8.0)

set -euo pipefail

# ---------------------------------------------------------------------------
# PINNED VERSIONS — keep these matched to the repo files noted above.
# ---------------------------------------------------------------------------

# Exact nightly from rust-toolchain.toml. A floating "nightly" drifts and
# silently desyncs the sandbox from CI, so we pin the same date CI resolves to.
PINNED_NIGHTLY="nightly-2026-07-15"

# rustup components and bare-metal targets, mirrored from rust-toolchain.toml.
RUST_COMPONENTS="rust-src rustfmt clippy llvm-tools"
RUST_TARGETS="x86_64-unknown-none aarch64-unknown-none"

# System packages the build/test pipeline shells out to. Same list as the CI
# "Install system dependencies" step in build.yml. mdbook is added on top for
# `cargo xtask docs` (CI builds docs with a separate action, but a full dev
# sandbox wants it locally).
APT_PACKAGES="qemu-system-x86 xorriso squashfs-tools e2fsprogs"

# Sisyphus multi-agent tooling, pinned to my local version. Its npm postinstall
# wires ~/.claude, reproducing the /plan -> prometheus -> /review -> momus flow
# with plans under .sisyphus/. The old curl|bash installer is dead (the project
# was rebranded to oh-my-claudecode), so we install strictly via npm.
SISYPHUS_PKG="oh-my-claude-sisyphus@1.8.0"

# Base image warmed for the Docker-based Linux-CI reproduction workflow. The
# repo has no committed compose file, so this is a fixed pull of the CI runner
# base (ubuntu-latest == 24.04) used to reproduce Linux-only ext2/e2fsprogs
# behaviour that does not surface on the macOS host.
DOCKER_WARM_IMAGE="ubuntu:24.04"

log() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }

# ---------------------------------------------------------------------------
# 1. System packages (apt).
# ---------------------------------------------------------------------------
# We refresh the apt index once, then install anything missing. apt-get install
# is itself idempotent, but we still gate on dpkg so a warm re-run skips the
# network round-trip entirely.

log "Installing system dependencies: ${APT_PACKAGES}"
export DEBIAN_FRONTEND=noninteractive

missing_pkgs=""
for pkg in ${APT_PACKAGES}; do
  if ! dpkg -s "${pkg}" >/dev/null 2>&1; then
    missing_pkgs="${missing_pkgs} ${pkg}"
  fi
done

if [ -n "${missing_pkgs}" ]; then
  apt-get update -y
  # shellcheck disable=SC2086 # word-splitting the package list is intentional
  apt-get install -y ${missing_pkgs}
else
  echo "All apt packages already present; skipping."
fi

# Baseline build tooling that the rustup installer and cargo need. curl is used
# to fetch rustup; build-essential/pkg-config cover native crates in xtask.
for pkg in curl ca-certificates build-essential pkg-config; do
  if ! dpkg -s "${pkg}" >/dev/null 2>&1; then
    apt-get update -y
    apt-get install -y "${pkg}"
  fi
done

# ---------------------------------------------------------------------------
# 2. Rust toolchain (rustup + pinned nightly).
# ---------------------------------------------------------------------------
# Running as root, rustup installs to /root/.rustup and /root/.cargo. We install
# with --default-toolchain none and then add the exact nightly ourselves so the
# pinned date — not "latest nightly" — is what lands. When the repo is later
# cloned, its rust-toolchain.toml resolves to this same already-installed
# toolchain, so no second download happens at first build.

export RUSTUP_HOME="${RUSTUP_HOME:-/root/.rustup}"
export CARGO_HOME="${CARGO_HOME:-/root/.cargo}"

if ! command -v rustup >/dev/null 2>&1; then
  log "Installing rustup (no default toolchain)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain none --profile minimal
else
  echo "rustup already installed; skipping installer."
fi

# Make cargo/rustup available in THIS shell for the remaining steps.
# shellcheck disable=SC1091
. "${CARGO_HOME}/env"

if ! rustup toolchain list | grep -q "${PINNED_NIGHTLY}"; then
  log "Installing Rust ${PINNED_NIGHTLY} with components + bare-metal targets"
  # `rustup toolchain install` takes exactly ONE value per --component/--target
  # flag; a space-separated list would be misparsed as extra positional
  # toolchain names (`invalid toolchain name: 'rustfmt'`). So build repeated
  # flags — one per component/target — into an argument array.
  install_args=(--profile minimal)
  for comp in ${RUST_COMPONENTS}; do
    install_args+=(--component "${comp}")
  done
  for tgt in ${RUST_TARGETS}; do
    install_args+=(--target "${tgt}")
  done
  rustup toolchain install "${PINNED_NIGHTLY}" "${install_args[@]}"
else
  echo "Toolchain ${PINNED_NIGHTLY} already installed; ensuring components/targets."
  # Adding an already-present component/target is a no-op, so this is safe to
  # run every time and repairs a partially-installed toolchain.
  # shellcheck disable=SC2086
  rustup component add --toolchain "${PINNED_NIGHTLY}" ${RUST_COMPONENTS} || true
  # shellcheck disable=SC2086
  rustup target add --toolchain "${PINNED_NIGHTLY}" ${RUST_TARGETS} || true
fi

# Make the pinned nightly the default so bare `cargo`/`rustc` outside the repo
# also use it (inside the repo, rust-toolchain.toml already forces it).
rustup default "${PINNED_NIGHTLY}"

# mdbook for `cargo xtask docs`. Best-effort: a docs-tool failure must not abort
# the whole environment. Installed via cargo so the version tracks the current
# toolchain rather than a stale apt package.
if ! command -v mdbook >/dev/null 2>&1; then
  log "Installing mdbook (best-effort, for cargo xtask docs)"
  cargo install mdbook --locked || echo "mdbook install failed; continuing."
else
  echo "mdbook already installed; skipping."
fi

# ---------------------------------------------------------------------------
# 3. Node.js + Sisyphus multi-agent tooling.
# ---------------------------------------------------------------------------
# Sisyphus is an npm global whose postinstall provisions ~/.claude. We install
# Node/npm from apt if absent, then the pinned Sisyphus package.

if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
  log "Installing Node.js + npm"
  apt-get update -y
  apt-get install -y nodejs npm
else
  echo "Node.js + npm already present ($(node --version)); skipping."
fi

# Gate on the exact pinned version so re-runs don't reinstall, but an upgrade of
# the pin still triggers a fresh install.
sisyphus_ver="${SISYPHUS_PKG##*@}"
if npm ls -g --depth=0 "${SISYPHUS_PKG%@*}" 2>/dev/null | grep -q "@${sisyphus_ver}"; then
  echo "${SISYPHUS_PKG} already installed; skipping."
else
  log "Installing Sisyphus (${SISYPHUS_PKG}) — wires ~/.claude via postinstall"
  npm i -g "${SISYPHUS_PKG}"
fi

# ---------------------------------------------------------------------------
# 4. Warm Docker test image (best-effort).
# ---------------------------------------------------------------------------
# Pre-pull the Linux-CI reproduction base so the first `docker run` in the
# sandbox is instant. Docker may be absent or the daemon unreachable at
# env-create time; neither is fatal.

if command -v docker >/dev/null 2>&1; then
  log "Warming Docker image ${DOCKER_WARM_IMAGE} (best-effort)"
  if docker image inspect "${DOCKER_WARM_IMAGE}" >/dev/null 2>&1; then
    echo "${DOCKER_WARM_IMAGE} already pulled; skipping."
  else
    docker pull "${DOCKER_WARM_IMAGE}" || echo "docker pull failed; continuing."
  fi
else
  echo "Docker not available; skipping image warm."
fi

log "cloud-setup.sh complete."

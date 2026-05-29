# ThemeliOS macOS host dependencies.
#
# These are the non-Rust tools the build/test pipeline shells out to. Install
# them all in one shot with:
#
#     brew bundle
#
# (Rust itself is managed by rustup + rust-toolchain.toml, not Homebrew.)
# On Linux, install the equivalents with your package manager — see
# docs/src/dev-setup.md.

# QEMU — emulates the hardware ThemeliOS boots on (amd64 + arm64).
# Provides qemu-system-x86_64 and qemu-system-aarch64.
brew "qemu"

# xorriso — builds the hybrid BIOS+UEFI bootable ISO (cargo xtask iso/run).
brew "xorriso"

# squashfs — provides mksquashfs/unsquashfs for the compressed read-only root
# filesystem image (cargo xtask image, Phase 3 storage).
brew "squashfs"

# e2fsprogs — provides mkfs.ext2 for the read-write data volume image
# (cargo xtask image, Phase 3 storage). Keg-only on macOS; xtask resolves the
# keg path automatically, so no PATH changes are needed.
brew "e2fsprogs"

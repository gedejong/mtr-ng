# Distributing mtr-ng

For packagers, and for whoever cuts a release.

## Package information

- **Name**: mtr-ng
- **Description**: Modern My Traceroute with real-time network path visualization
- **License**: MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`; ship both)
- **Homepage**: https://github.com/gedejong/mtr-ng
- **Category**: network utilities
- **Privileges**: none on macOS; root or `CAP_NET_RAW` on Linux (see below)

## What a release contains

Releases are built by [dist](https://github.com/axodotdev/cargo-dist) from `dist-workspace.toml`. Releases up to 0.3.0 predate it and had differently named assets.

| Asset | Contents |
|---|---|
| `mtr-ng-<target>.tar.xz` | binary, `mtr-ng.1`, README, both licences |
| `mtr-ng-<target>.tar.xz.sha256`, `sha256.sum` | checksums |
| `mtr-ng-installer.sh` | downloads the right archive into `~/.cargo/bin` |
| `mtr-ng_<version>-1_{amd64,arm64}.deb` (+ `.sha256`) | Debian package from `[package.metadata.deb]` |
| `source.tar.gz` | source |

Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` (static, for Alpine). The glibc builds and the `.deb`s are built on Ubuntu 22.04 and need glibc 2.35 or newer.

Windows is not shipped: it compiles but can't probe yet. FreeBSD is type-checked in CI only.

The formula is pushed to [gedejong/homebrew-tap](https://github.com/gedejong/homebrew-tap). Publishing to crates.io is paused (BACKLOG C10).

Up to 0.3.0 the repository, the tap and the crate belonged to `edejong-dbc`, an account that can no longer be reached. They stay at 0.3.0; users of `edejong-dbc/tap` have to switch taps.

## Installing

```bash
# Shell installer (macOS, Linux)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/gedejong/mtr-ng/releases/latest/download/mtr-ng-installer.sh | sh

# Homebrew (macOS, Linux)
brew install gedejong/tap/mtr-ng

# Debian/Ubuntu
sudo dpkg -i mtr-ng_*_amd64.deb

# Cargo (crates.io is stuck at 0.3.0 until C10)
cargo install --locked --git https://github.com/gedejong/mtr-ng
```

`packaging/PKGBUILD` is an unmaintained draft; mtr-ng is not on the AUR.

## Privileges

- **macOS**: unprivileged ICMP datagram sockets; no sudo, no setuid.
- **Linux**: raw sockets, so root or the `CAP_NET_RAW` file capability: `sudo setcap cap_net_raw+ep /usr/bin/mtr-ng` (or `scripts/setup-capabilities.sh`). A capability is narrower than setuid root. Reinstalling the binary drops it.
- **Windows**: not supported yet.

Packagers on Linux can set the capability in a post-install script, as Debian's `mtr` does. The `.deb` doesn't do this yet (BACKLOG X4).

## Cutting a release

1. Make sure CI on `master` is green.
2. `scripts/bump-version.sh X.Y.Z`. It runs the CI gate locally, sets the version in `Cargo.toml` and `Cargo.lock`, regenerates the man page, commits, tags `vX.Y.Z` and, after one confirmation, pushes the commit and the tag together.
3. The tag runs `.github/workflows/release.yml`. It builds every target and the `.deb`s, creates the GitHub release with all assets, then pushes the formula to the tap. Nothing is released unless every build succeeds.

Repository secrets the release needs:

- `HOMEBREW_TAP_TOKEN_PAT`: a token of gedejong's with write access to `gedejong/homebrew-tap` (fine-grained: *Contents: read and write* on that repository).
- `CARGO_REGISTRY_TOKEN`, once crates.io publishing is back (C10): a crates.io token with the *publish-update* scope for `mtr-ng`.

`release.yml` is generated: after changing `dist-workspace.toml`, run `dist generate` (dist 0.32, `cargo install cargo-dist --locked`) rather than editing it. `build-deb.yml` and `publish-homebrew.yml` are ours; dist calls them. `publish-crates.yml` is ours too, but left out of `publish-jobs` until C10. `publish-homebrew.yml` replaces dist's own Homebrew job so the formula also installs the man page; it fails rather than publish without it if dist's formula template changes. On pull requests the workflow only runs `dist plan`.

To check a release locally: `dist plan`, or `dist build --artifacts=local --target aarch64-apple-darwin` and look in `target/distrib/`.

## Contact

- GitHub issues: https://github.com/gedejong/mtr-ng/issues
- Email: edejong@fastmail.fm

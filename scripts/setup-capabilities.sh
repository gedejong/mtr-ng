#!/bin/bash
#
# Grant CAP_NET_RAW to an installed mtr-ng binary so it can probe without sudo
# on Linux.
#
# A file capability is stored on one file. Rebuilding or reinstalling replaces
# that file and drops the capability, so this script works on an installed
# binary, never on cargo's target/ directory. Rerun it after every reinstall.
#
# macOS needs none of this: mtr-ng uses unprivileged ICMP sockets there.

set -eo pipefail

if [ -t 1 ]; then
    RED=$'\033[0;31m'
    GREEN=$'\033[0;32m'
    YELLOW=$'\033[1;33m'
    BLUE=$'\033[0;34m'
    NC=$'\033[0m'
else
    RED='' GREEN='' YELLOW='' BLUE='' NC=''
fi

log() { printf '%s[INFO]%s %s\n' "$BLUE" "$NC" "$*"; }
success() { printf '%s[OK]%s %s\n' "$GREEN" "$NC" "$*"; }
warning() { printf '%s[WARNING]%s %s\n' "$YELLOW" "$NC" "$*" >&2; }
error() { printf '%s[ERROR]%s %s\n' "$RED" "$NC" "$*" >&2; }

show_help() {
    cat <<'EOF'
Usage: scripts/setup-capabilities.sh [-y] [PATH]

Grant CAP_NET_RAW to an installed mtr-ng binary so it runs without sudo on Linux.

PATH defaults to the mtr-ng found on $PATH, then ~/.cargo/bin/mtr-ng.
The capability is lost whenever the file is replaced (cargo install, a new
release, a rebuild copied over it), so rerun this script after reinstalling.

Options:
  -y, --yes     Don't ask for confirmation
  -h, --help    Show this help

Examples:
  # Per-user install
  cargo install --locked --path .
  scripts/setup-capabilities.sh

  # System-wide install
  cargo build --release --locked
  sudo install -m 755 target/release/mtr-ng /usr/local/bin/mtr-ng
  scripts/setup-capabilities.sh /usr/local/bin/mtr-ng
EOF
}

ASSUME_YES=0
BINARY=""

while [ $# -gt 0 ]; do
    case "$1" in
        -h | --help)
            show_help
            exit 0
            ;;
        -y | --yes)
            ASSUME_YES=1
            ;;
        -*)
            error "Unknown option: $1"
            show_help >&2
            exit 1
            ;;
        *)
            if [ -n "$BINARY" ]; then
                error "Only one PATH may be given"
                exit 1
            fi
            BINARY=$1
            ;;
    esac
    shift
done

# Run a command as root, through sudo unless we already are root.
as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

check_platform() {
    # Test the kernel, not $OSTYPE: that is linux-musl on Alpine and other musl systems.
    local kernel
    kernel=$(uname -s)
    if [ "$kernel" = "Linux" ]; then
        return
    fi
    error "File capabilities are a Linux feature; this system runs $kernel."
    if [ "$kernel" = "Darwin" ]; then
        echo "On macOS mtr-ng needs no privileges: it uses unprivileged ICMP sockets."
    else
        echo "Run mtr-ng with sudo instead."
    fi
    exit 1
}

require_tool() {
    if command -v "$1" >/dev/null 2>&1; then
        return
    fi
    error "$1 not found. Install the libcap tools:"
    echo "  Debian/Ubuntu: sudo apt install libcap2-bin"
    echo "  Fedora/RHEL:   sudo dnf install libcap"
    echo "  Arch Linux:    sudo pacman -S libcap"
    echo "  Alpine:        sudo apk add libcap-utils"
    exit 1
}

resolve_binary() {
    if [ -z "$BINARY" ]; then
        BINARY=$(command -v mtr-ng || true)
        if [ -z "$BINARY" ]; then
            BINARY="$HOME/.cargo/bin/mtr-ng"
        fi
    fi
    if [ ! -f "$BINARY" ]; then
        error "No mtr-ng binary at $BINARY."
        echo "Install it first, then rerun this script:"
        echo "  cargo install --locked --path .   # from a checkout"
        echo "  cargo install --locked mtr-ng     # from crates.io"
        exit 1
    fi
    # setcap acts on the file itself, so follow symlinks to it.
    BINARY=$(readlink -f "$BINARY")

    case "$BINARY" in
        */target/debug/* | */target/release/* | */target/*/debug/* | */target/*/release/*)
            warning "$BINARY is cargo build output: the next build replaces it and drops the capability."
            warning "Install a copy instead (cargo install --locked --path .) and pass that path."
            ;;
    esac
    local mount_options=""
    if command -v findmnt >/dev/null 2>&1; then
        mount_options=$(findmnt -n -o OPTIONS -T "$BINARY" 2>/dev/null || true)
    fi
    case ",$mount_options," in
        *,nosuid,*)
            warning "$BINARY is on a nosuid filesystem, where the kernel ignores file capabilities."
            ;;
    esac
}

explain() {
    cat <<EOF

This grants CAP_NET_RAW to one file:
  $BINARY

That is narrower than running mtr-ng with sudo. sudo gives the whole program
(terminal UI, DNS resolver, argument parsing) every root privilege; the
capability gives it raw sockets and nothing else. ping and mtr-packet are
installed the same way by most distributions.

What it does allow: anyone who can execute this file can open raw sockets
through it. If mtr-ng had an exploitable bug, an attacker could use it to
send arbitrary packets or read network traffic, but not to gain root. mtr-ng
does not yet drop the capability after opening its sockets.

EOF
}

confirm() {
    if [ "$ASSUME_YES" -eq 1 ]; then
        return
    fi
    if [ ! -t 0 ]; then
        error "No terminal to ask for confirmation; rerun with --yes."
        exit 1
    fi
    local reply
    read -r -p "Grant CAP_NET_RAW to $BINARY? [y/N] " reply
    case "$reply" in
        [Yy] | [Yy][Ee][Ss]) ;;
        *)
            log "Aborted; nothing changed."
            exit 0
            ;;
    esac
}

set_capability() {
    log "Setting cap_net_raw+ep on $BINARY"
    if ! as_root setcap cap_net_raw+ep "$BINARY"; then
        error "setcap failed"
        exit 1
    fi
}

verify_capability() {
    local caps re
    caps=$(getcap "$BINARY")
    # libcap before 2.41 prints "PATH = cap_net_raw+ep"; later versions print
    # "PATH cap_net_raw=ep", possibly grouped with others ("cap_net_admin,cap_net_raw=ep").
    re='cap_net_raw(,[a-z_]+)*[+=]ei?p'
    if [[ $caps =~ $re ]]; then
        success "Capability set: $caps"
    else
        error "Capability not set as expected: ${caps:-<none>}"
        exit 1
    fi
}

test_binary() {
    if [ "$(id -u)" -eq 0 ]; then
        warning "Running as root, so a test run here proves nothing. As a normal user, try:"
        echo "  $BINARY -r -c 1 -n 127.0.0.1"
        return
    fi
    log "Probing 127.0.0.1 without sudo..."
    local output
    # Hop 1 only shows 127.0.0.1 if a reply came back.
    if output=$(timeout 15 "$BINARY" -r -c 1 -n 127.0.0.1 2>&1) &&
        grep -Eq '1\.\|-- +127\.0\.0\.1' <<<"$output"; then
        success "mtr-ng probes without sudo."
    else
        warning "The test run failed. Run it by hand to see the error:"
        echo "  $BINARY -r -c 1 -n 127.0.0.1"
        exit 1
    fi
}

check_platform
require_tool setcap
require_tool getcap
resolve_binary
explain
confirm
set_capability
verify_capability
test_binary

echo
success "Done. Run mtr-ng without sudo, for example:"
echo "  $BINARY example.com"
echo
log "Reinstalling replaces the file and drops the capability; rerun this script afterwards."

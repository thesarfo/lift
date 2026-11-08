#!/usr/bin/env bash
# install or remove lift for the current user.
#   ./install.sh              install (uses a prebuilt ./lift if present, otherwise builds)
#   ./install.sh uninstall    remove it
set -euo pipefail

cd "$(dirname "$0")"

prefix="${PREFIX:-$HOME/.local}"
bin="$prefix/bin/lift"

case "${1:-install}" in
install)
    if [ -x ./lift ]; then
        src=./lift
    else
        cargo build --release
        src=target/release/lift
    fi
    install -Dm755 "$src" "$bin"
    echo "installed $bin"
    case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "add $prefix/bin to your PATH to run: lift" ;;
    esac
    ;;
uninstall)
    rm -f "$bin"
    echo "removed lift"
    ;;
*)
    echo "usage: $0 [install|uninstall]" >&2
    exit 2
    ;;
esac

#!/bin/sh
# OptMem installer. Run it again to update: it only replaces the binary, and
# `memo init` never touches memories that already exist.
#
#   curl -fsSL https://raw.githubusercontent.com/lennix1337/OptMem/main/install.sh | sh

set -e
DIR="$HOME/.optmem"
REPO="${OPTMEM_REPO:-https://github.com/lennix1337/OptMem}"

command -v cargo >/dev/null || {
  echo "OptMem builds from source, and this machine has no cargo." >&2
  echo "Install Rust from https://rustup.rs, then run this line again." >&2
  exit 1
}

case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=.exe ;; *) EXE= ;; esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
cargo install --quiet --locked --git "$REPO" --root "$TMP" optmem
mkdir -p "$DIR"
# Windows will not overwrite a running .exe, but it lets one be renamed.
[ -e "$DIR/memo$EXE" ] && mv -f "$DIR/memo$EXE" "$DIR/memo.old$EXE"
mv "$TMP/bin/memo$EXE" "$DIR/memo$EXE"
rm -f "$DIR/memo.old$EXE" 2>/dev/null || true
if [ -n "$EXE" ]; then
  # The Python tool's leftovers would shadow memo.exe: bash runs an
  # extensionless `memo` first, and cmd runs memo.cmd.
  rm -f "$DIR/memo" "$DIR/memo.cmd" "$DIR/memo.ps1"
fi

exec "$DIR/memo$EXE" init

#!/usr/bin/env bash
# install-socket-vmnet.sh — copy socket_vmnet into /opt/socket_vmnet/bin/
#
# Lima's sudoers (/etc/sudoers.d/lima) and our LaunchDaemon plist both name
# this exact path. The binary must be there, root-owned, mode 0755. Most
# package managers leave it elsewhere — this script bridges them to the
# canonical location.
#
# Sources (auto-detected, first match wins):
#   1. $1 (explicit path passed as first argument)
#   2. /usr/local/bin/socket_vmnet            (pkgm install symlink)
#   3. /opt/homebrew/opt/socket_vmnet/bin/    (brew keg-only install)
#   4. `command -v socket_vmnet` in PATH
#
# Idempotent: skip if /opt/socket_vmnet/bin/socket_vmnet already matches
# the source byte-for-byte. Will prompt for sudo password on first install
# (unless you've added an appropriate NOPASSWD rule).

set -eu

DEST_DIR=/opt/socket_vmnet/bin
DEST="$DEST_DIR/socket_vmnet"

# 1. Resolve source.
SRC="${1:-}"
if [ -z "$SRC" ]; then
  for candidate in \
      /usr/local/bin/socket_vmnet \
      /opt/homebrew/opt/socket_vmnet/bin/socket_vmnet; do
    if [ -x "$candidate" ] && [ "$candidate" != "$DEST" ]; then
      SRC="$candidate"; break
    fi
  done
fi
if [ -z "$SRC" ]; then
  SRC=$(command -v socket_vmnet 2>/dev/null || true)
  if [ -z "$SRC" ] || [ "$SRC" = "$DEST" ]; then
    cat >&2 <<EOF
[install] socket_vmnet not found outside $DEST_DIR.

Install it first (one of):
  brew install socket_vmnet
  pkgm install github.com/lima-vm/socket_vmnet   # once pkgxdev/pantry#13093 lands
  git clone https://github.com/lima-vm/socket_vmnet && cd socket_vmnet && make

Then re-run:
  $0                 # auto-detect source
  $0 /path/to/socket_vmnet   # explicit source
EOF
    exit 1
  fi
fi
if [ ! -x "$SRC" ]; then
  echo "[install] $SRC is not executable" >&2
  exit 1
fi
echo "[install] source = $SRC"

# 2. Idempotence.
if [ -x "$DEST" ] && cmp -s "$SRC" "$DEST"; then
  echo "[install] $DEST already matches source — nothing to do"
  exit 0
fi

# 3. Install. sudo prompt may appear here.
echo "[install] copying to $DEST (sudo password may be requested)"
sudo install -m 0755 -o root -g wheel -d "$DEST_DIR"
sudo install -m 0755 -o root -g wheel "$SRC" "$DEST"

# 4. socket_vmnet_client (optional sibling).
SRC_DIR=$(dirname "$SRC")
if [ -x "$SRC_DIR/socket_vmnet_client" ]; then
  sudo install -m 0755 -o root -g wheel \
    "$SRC_DIR/socket_vmnet_client" "$DEST_DIR/socket_vmnet_client"
  echo "[install] socket_vmnet_client installed too"
fi

ls -la "$DEST_DIR/"
echo "[install] done — start the daemon however you prefer:"
echo "  sudo brew services start socket_vmnet"
echo "  # or via a LaunchDaemon plist"
echo "  # or one-shot via the same args Lima would use"

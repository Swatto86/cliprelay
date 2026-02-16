#!/usr/bin/env bash
set -euo pipefail

# Installs and enables the ClipRelay relay as a systemd service.
#
# Usage:
#   sudo ./install-relay-systemd.sh --binary ./cliprelay-relay --bind-address 127.0.0.1:8080
#
# Defaults:
#   --binary: ./cliprelay-relay
#   --bind-address: 127.0.0.1:8080
#
# Notes:
# - Use 127.0.0.1:8080 when running behind a reverse proxy (e.g., Caddy/Nginx) on :443.
# - Use 0.0.0.0:8080 for direct exposure (not recommended without TLS).

BINARY="./cliprelay-relay"
BIND_ADDRESS="127.0.0.1:8080"
RUST_LOG_VALUE="info"
SERVICE_NAME="cliprelay-relay.service"
INSTALL_DIR="/opt/cliprelay/bin"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary)
      BINARY="$2"; shift 2 ;;
    --bind-address)
      BIND_ADDRESS="$2"; shift 2 ;;
    --rust-log)
      RUST_LOG_VALUE="$2"; shift 2 ;;
    --install-dir)
      INSTALL_DIR="$2"; shift 2 ;;
    -h|--help)
      echo "Usage: sudo $0 [--binary PATH] [--bind-address HOST:PORT] [--rust-log LEVEL] [--install-dir DIR]";
      exit 0 ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 2 ;;
  esac
done

if [[ $EUID -ne 0 ]]; then
  echo "This script must be run as root (use sudo)." >&2
  exit 1
fi

if ! command -v systemctl >/dev/null 2>&1; then
  echo "systemctl not found; this host does not appear to use systemd." >&2
  exit 1
fi

if [[ ! -f "$BINARY" ]]; then
  echo "Relay binary not found: $BINARY" >&2
  echo "Build it first (on the server or via CI artifact), then re-run." >&2
  exit 1
fi

if [[ ! -x "$BINARY" ]]; then
  echo "Relay binary is not executable; fixing permissions." >&2
  chmod +x "$BINARY"
fi

# Create a dedicated service user
if ! id -u cliprelay >/dev/null 2>&1; then
  useradd --system --no-create-home --shell /usr/sbin/nologin cliprelay
fi

install -d -m 0755 /etc/cliprelay
cat > /etc/cliprelay/relay.env <<EOF
# Managed by install-relay-systemd.sh
CLIPRELAY_BIND_ADDRESS=${BIND_ADDRESS}
RUST_LOG=${RUST_LOG_VALUE}
EOF

# Install binary
install -d -m 0755 "${INSTALL_DIR}"
install -m 0755 "$BINARY" "${INSTALL_DIR}/cliprelay-relay"
chown root:root "${INSTALL_DIR}/cliprelay-relay"

# Install service
install -d -m 0755 /etc/systemd/system
install -m 0644 "$(dirname "$0")/cliprelay-relay.service" "/etc/systemd/system/${SERVICE_NAME}"

systemctl daemon-reload
systemctl enable --now "${SERVICE_NAME}"

echo "Installed and started: ${SERVICE_NAME}"
echo "Status:"
systemctl --no-pager --full status "${SERVICE_NAME}" || true

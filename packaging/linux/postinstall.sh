#!/usr/bin/env bash
# Post-install: create the _lowband system account, set file permissions,
# and enable the systemd service.
set -euo pipefail

DAEMON_USER="_lowband"
DAEMON_BIN="/usr/local/bin/lowbandd"
DATA_DIR="/var/lib/lowband"
LOG_DIR="/var/log/lowband"
SERVICE="lowbandd.service"

# ── Create least-privilege system account ────────────────────────────────────
create_system_user() {
    if id -u "$DAEMON_USER" &>/dev/null 2>&1; then
        return 0
    fi
    useradd \
        --system \
        --no-create-home \
        --home-dir /var/empty \
        --shell /usr/sbin/nologin \
        --comment "LowBand Daemon" \
        "$DAEMON_USER"
}

# ── Set file permissions ─────────────────────────────────────────────────────
set_permissions() {
    # Binary: root:root, 755 — readable and executable by all, writable only by root.
    chown root:root "$DAEMON_BIN"
    chmod 755 "$DAEMON_BIN"

    # Data dir: _lowband writes its SQLite stores here.
    install -d -m 750 -o "$DAEMON_USER" -g "$DAEMON_USER" "$DATA_DIR"

    # Log dir: root-owned; daemon appends via the systemd StandardOutput directive.
    install -d -m 755 -o root -g root "$LOG_DIR"
}

# ── Enable and start service ─────────────────────────────────────────────────
enable_service() {
    systemctl daemon-reload
    systemctl enable "$SERVICE"
    if systemctl is-active --quiet "$SERVICE"; then
        systemctl restart "$SERVICE"
    else
        systemctl start "$SERVICE"
    fi
}

create_system_user
set_permissions
enable_service

exit 0

#!/usr/bin/env bash
# switch2d installer: idempotent, works from any directory, no runtime root.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

RULES_SRC="$SCRIPT_DIR/system/70-switch2d.rules"
RULES_DEST="/etc/udev/rules.d/70-switch2d.rules"
PRIVACY_SCRIPT_SRC="$SCRIPT_DIR/system/disable-le-privacy.sh"
PRIVACY_SCRIPT_DEST="/usr/local/bin/disable-le-privacy.sh"
PRIVACY_UNIT_SRC="$SCRIPT_DIR/system/switch2d-disable-le-privacy.service"
PRIVACY_UNIT_DEST="/etc/systemd/system/switch2d-disable-le-privacy.service"
SUDOERS_DEST="/etc/sudoers.d/switch2d-btmgmt"
USER_UNIT_SRC="$SCRIPT_DIR/system/switch2d.service"
USER_UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
USER_UNIT_DEST="$USER_UNIT_DIR/switch2d.service"
INSTALL_USER="$(id -un)"
SUDOERS_RULE="$INSTALL_USER ALL=(root) NOPASSWD: /usr/bin/btmgmt -i 0 stop-find -l, /usr/bin/btmgmt -i 0 stop-find"
CARGO_BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
BIN="$CARGO_BIN_DIR/switch2d"

SERVICE=0
UNINSTALL=0

usage() {
    cat <<'EOF'
Usage: ./install.sh [--service | --uninstall]

Without flags, installs the switch2d binary, udev rule, LE-privacy unit and
least-privilege sudoers rule. Re-running is safe; unchanged steps are skipped.

  --service    also install and enable the user login service
  --uninstall  remove the udev rule, LE-privacy unit/script, sudoers rule and
               user unit; keeps config.json, pairings and the installed binary
  -h, --help   show this help
EOF
}

step() { printf '\n==> %s\n' "$*"; }
info() { printf '  %s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

check_not_root() {
    [[ ${EUID} -ne 0 ]] || die "run as your desktop user, not with sudo or as root"
}

check_sources() {
    local file
    for file in "$RULES_SRC" "$PRIVACY_SCRIPT_SRC" "$PRIVACY_UNIT_SRC" "$USER_UNIT_SRC"; do
        [[ -f "$file" ]] || die "missing $file (is this the switch2d repository?)"
    done
}

# Return 0 when the file changed, 1 when it was already identical.
install_system_file() {
    local src=$1 dest=$2 mode=$3
    if [[ -f "$dest" ]] && cmp -s "$src" "$dest"; then
        info "already installed: $dest"
        return 1
    fi
    info "installing $dest"
    sudo install -Dm"$mode" "$src" "$dest"
}

install_user_file() {
    local src=$1 dest=$2 mode=$3
    if [[ -f "$dest" ]] && cmp -s "$src" "$dest"; then
        info "already installed: $dest"
        return 1
    fi
    info "installing $dest"
    install -Dm"$mode" "$src" "$dest"
}

check_legacy_units() {
    command -v systemctl >/dev/null 2>&1 || return 0
    systemctl --user show-environment >/dev/null 2>&1 || return 0

    local units=(
        nso-gc.service
        nso-gc-watchdog.service
        nso-gc-watchdog.timer
        nso-gc-after-gamescope.service
    )
    local found=()
    local unit
    for unit in "${units[@]}"; do
        if systemctl --user is-enabled --quiet "$unit" 2>/dev/null \
            || systemctl --user is-active --quiet "$unit" 2>/dev/null; then
            found+=("$unit")
        fi
    done

    if ((${#found[@]} > 0)); then
        warn "legacy Python bridge units are still enabled/active: ${found[*]}"
        warn "they can restart the old bridge and compete for the adapter; disable them first:"
        for unit in "${found[@]}"; do
            printf '  systemctl --user disable --now %s\n' "$unit" >&2
        done
    fi
}

install_binary() {
    step "Installing switch2d with cargo (locked)"
    cargo install --path "$SCRIPT_DIR" --locked

    if [[ -x "$CARGO_BIN_DIR/switch2d" ]]; then
        BIN="$CARGO_BIN_DIR/switch2d"
    else
        BIN="$(command -v switch2d || true)"
    fi
    [[ -n "$BIN" && -x "$BIN" ]] || die "cargo finished but switch2d was not found"
    info "binary: $BIN"

    if ! command -v switch2d >/dev/null 2>&1; then
        warn "$CARGO_BIN_DIR is not on PATH; add it so the switch2d command works"
    fi
}

install_udev_rule() {
    step "Installing udev rule (uinput/evdev access and player LEDs)"
    if install_system_file "$RULES_SRC" "$RULES_DEST" 0644; then
        sudo udevadm control --reload-rules
        sudo udevadm trigger
    else
        info "udev rule unchanged; no reload needed"
    fi
}

install_privacy_unit() {
    step "Installing LE-privacy unit (required for controller reconnects)"
    local changed=0
    if install_system_file "$PRIVACY_SCRIPT_SRC" "$PRIVACY_SCRIPT_DEST" 0755; then
        changed=1
    fi
    if install_system_file "$PRIVACY_UNIT_SRC" "$PRIVACY_UNIT_DEST" 0644; then
        changed=1
    fi
    if ((changed)); then
        sudo systemctl daemon-reload
    fi

    local enabled active
    enabled="$(systemctl is-enabled switch2d-disable-le-privacy.service 2>/dev/null || true)"
    active="$(systemctl is-active switch2d-disable-le-privacy.service 2>/dev/null || true)"
    if [[ "$enabled" == "enabled" && "$active" == "active" && $changed -eq 0 ]]; then
        info "already enabled and active"
        return
    fi

    sudo systemctl enable --now switch2d-disable-le-privacy.service
    if ((changed)); then
        # RemainAfterExit units do not rerun on daemon-reload alone.
        sudo systemctl restart switch2d-disable-le-privacy.service
    fi
}

install_sudoers_rule() {
    step "Installing least-privilege sudoers rule"
    require_command visudo
    local visudo_bin
    visudo_bin="$(command -v visudo)"
    if ! sudo test -d /etc/sudoers.d; then
        sudo install -d -m 0750 /etc/sudoers.d
    fi

    local current
    current="$(sudo cat "$SUDOERS_DEST" 2>/dev/null || true)"
    if [[ "$current" == "$SUDOERS_RULE" ]]; then
        info "already installed: $SUDOERS_DEST"
        return
    fi

    info "installing $SUDOERS_DEST"
    printf '%s\n' "$SUDOERS_RULE" | sudo tee "$SUDOERS_DEST" >/dev/null
    sudo chmod 0440 "$SUDOERS_DEST"
    sudo "$visudo_bin" -cf "$SUDOERS_DEST"
}

install_user_service() {
    step "Installing user login service (--service)"
    local changed=0
    if install_user_file "$USER_UNIT_SRC" "$USER_UNIT_DEST" 0644; then
        changed=1
    fi

    if ! systemctl --user show-environment >/dev/null 2>&1; then
        warn "user systemd is not available; unit installed but not enabled"
        return
    fi
    if ((changed)); then
        systemctl --user daemon-reload
    fi

    if systemctl --user is-enabled --quiet switch2d.service 2>/dev/null && ((changed == 0)); then
        info "already enabled: switch2d.service"
    else
        systemctl --user enable switch2d.service
    fi
}

run_doctor() {
    step "Running switch2d doctor"
    if ! "$BIN" doctor; then
        warn "doctor reported FAIL; fix the lines above before pairing or running the daemon"
        exit 1
    fi
}

do_install() {
    check_not_root
    require_command cargo
    require_command sudo
    require_command install
    require_command cmp
    require_command systemctl
    require_command udevadm
    check_sources

    check_legacy_units

    install_binary
    install_udev_rule
    install_privacy_unit
    install_sudoers_rule
    if ((SERVICE)); then
        install_user_service
    else
        step "Skipping user login service (pass --service to enable it)"
    fi
    run_doctor

    printf '\ninstall.sh: done. Run `switch2d run` in the foreground, or use the user service.\n'
}

do_uninstall() {
    check_not_root
    require_command sudo
    require_command systemctl
    require_command udevadm

    step "Removing LE-privacy unit and script"
    if sudo test -e "$PRIVACY_UNIT_DEST"; then
        sudo systemctl disable --now switch2d-disable-le-privacy.service 2>/dev/null || true
        sudo rm -f "$PRIVACY_UNIT_DEST"
        sudo systemctl daemon-reload
        info "removed $PRIVACY_UNIT_DEST"
    else
        info "not installed: $PRIVACY_UNIT_DEST"
    fi
    if sudo test -e "$PRIVACY_SCRIPT_DEST"; then
        sudo rm -f "$PRIVACY_SCRIPT_DEST"
        info "removed $PRIVACY_SCRIPT_DEST"
    else
        info "not installed: $PRIVACY_SCRIPT_DEST"
    fi

    step "Removing udev rule"
    if sudo test -e "$RULES_DEST"; then
        sudo rm -f "$RULES_DEST"
        sudo udevadm control --reload-rules
        sudo udevadm trigger
        info "removed $RULES_DEST"
    else
        info "not installed: $RULES_DEST"
    fi

    step "Removing sudoers rule"
    if sudo test -e "$SUDOERS_DEST"; then
        sudo rm -f "$SUDOERS_DEST"
        info "removed $SUDOERS_DEST"
    else
        info "not installed: $SUDOERS_DEST"
    fi

    step "Removing user service"
    if systemctl --user show-environment >/dev/null 2>&1; then
        systemctl --user disable --now switch2d.service 2>/dev/null || true
    fi
    if [[ -e "$USER_UNIT_DEST" ]]; then
        rm -f "$USER_UNIT_DEST"
        if systemctl --user show-environment >/dev/null 2>&1; then
            systemctl --user daemon-reload
        fi
        info "removed $USER_UNIT_DEST"
    else
        info "not installed: $USER_UNIT_DEST"
    fi

    info "kept: $HOME/.config/switch2d/config.json and controller pairings"
    info "kept: $BIN (remove with \`cargo uninstall switch2d\` if you want)"
}

while (($# > 0)); do
    case "$1" in
        --service) SERVICE=1 ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
    shift
done

if ((SERVICE && UNINSTALL)); then
    die "--service and --uninstall cannot be combined"
fi

if ((UNINSTALL)); then
    do_uninstall
else
    do_install
fi

#!/usr/bin/env bash
# Lunaris agent installer for Linux (x86_64), made by the Lunaris server.
#
#   curl -fsSL https://SERVER/install/agent.sh | sudo LUNARIS_INSTALL_TOKEN='TOKEN' bash -s -- [--name NAME]
#   (or: ... | sudo bash -s -- --token TOKEN [--name NAME])
#
# Installs the Lunaris agent of this server's version from GitHub Releases
# (the .deb with apt, otherwise the AppImage in /opt/lunaris-agent), joins it
# to the server with the install token (the token is used once, then the
# agent signs in with its own key) and starts it at the desktop user's login.
# It also enables the lunaris-capture service (root, systemd): unattended
# screen capture on Wayland without the portal prompt.
#
# Options:
#   --token TOKEN   install token from the server's web page (or the
#                   LUNARIS_INSTALL_TOKEN environment variable)
#   --name NAME     name shown for this computer (default: its host name)
#   --user USER     desktop user the agent runs for (default: the one who ran sudo)
#   --appimage      use the AppImage even where apt is available
set -euo pipefail

SERVER_URL='@@SERVER_URL@@'
VERSION='@@VERSION@@'
CERT_SHA256='@@CERT_SHA256@@'
RELEASE_BASE="https://github.com/collyn/lunaris/releases/download/v${VERSION}"
INSTALL_DIR=/opt/lunaris-agent

die() { echo "error: $*" >&2; exit 1; }
info() { echo "==> $*"; }

TOKEN="${LUNARIS_INSTALL_TOKEN:-}"
NAME=""
TARGET_USER="${SUDO_USER:-}"
FORCE_APPIMAGE=0
while [ $# -gt 0 ]; do
    case "$1" in
        --token) [ $# -ge 2 ] || die "--token needs a value"; TOKEN="$2"; shift 2 ;;
        --token=*) TOKEN="${1#*=}"; shift ;;
        --name) [ $# -ge 2 ] || die "--name needs a value"; NAME="$2"; shift 2 ;;
        --name=*) NAME="${1#*=}"; shift ;;
        --user) [ $# -ge 2 ] || die "--user needs a value"; TARGET_USER="$2"; shift 2 ;;
        --user=*) TARGET_USER="${1#*=}"; shift ;;
        --appimage) FORCE_APPIMAGE=1; shift ;;
        -h|--help) sed -n '2,17p' "$0" 2>/dev/null || true; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

[ -n "$TOKEN" ] || die "missing install token: set LUNARIS_INSTALL_TOKEN or pass --token (copy the command from the server's web page)"
[[ "$TOKEN" =~ ^lit_[A-Za-z0-9_-]{16,128}$ ]] || die "this is not an install token (they start with lit_)"
[ "${#NAME}" -le 128 ] || die "--name is longer than 128 characters"
[ "$(uname -s)" = Linux ] || die "this installer is for Linux"
[ "$(uname -m)" = x86_64 ] || die "only x86_64 (amd64) computers are supported, this one is $(uname -m)"
[ "$(id -u)" -eq 0 ] || die "run as root: curl -fsSL <url>/install/agent.sh | sudo LUNARIS_INSTALL_TOKEN=<token> bash -s"

if [ -z "$TARGET_USER" ] || [ "$TARGET_USER" = root ]; then
    die "the agent runs in a desktop user's graphical session, not as root. Run this with sudo from that user's account, or add --user <name>."
fi
TARGET_HOME="$(getent passwd "$TARGET_USER" | cut -d: -f6 || true)"
[ -n "$TARGET_HOME" ] && [ -d "$TARGET_HOME" ] || die "user $TARGET_USER has no home folder"
command -v curl >/dev/null || die "curl is needed"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
# apt reads the package as its own sandbox user.
chmod 755 "$tmp"

download() {
    info "Downloading $1"
    curl -fL --proto '=https' --tlsv1.2 --retry 3 --progress-bar -o "$2" "$RELEASE_BASE/$1" \
        || die "cannot download $RELEASE_BASE/$1 (is version $VERSION released?)"
}

if [ "$FORCE_APPIMAGE" -eq 0 ] && command -v apt-get >/dev/null && command -v dpkg >/dev/null; then
    deb="lunaris-agent_${VERSION}_amd64.deb"
    download "$deb" "$tmp/$deb"
    chmod 644 "$tmp/$deb"
    info "Installing $deb"
    DEBIAN_FRONTEND=noninteractive apt-get install -y "$tmp/$deb"
    AGENT_BIN=/usr/bin/lunaris-agent
else
    appimage="lunaris-agent-linux-x86_64.AppImage"
    download "$appimage" "$tmp/$appimage"
    info "Installing $INSTALL_DIR/lunaris-agent.AppImage"
    install -d -m 755 "$INSTALL_DIR"
    install -m 755 "$tmp/$appimage" "$INSTALL_DIR/lunaris-agent.AppImage"
    ln -sfn "$INSTALL_DIR/lunaris-agent.AppImage" /usr/local/bin/lunaris-agent
    AGENT_BIN="$INSTALL_DIR/lunaris-agent.AppImage"
    # Remote keyboard/mouse through /dev/uinput (what the packages install,
    # scripts/linux-host/setup-host.sh).
    cat > /etc/udev/rules.d/60-lunaris-input.rules <<'RULES'
# Lunaris host agent: virtual keyboard/mouse through /dev/uinput.
KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput", GROUP="input", MODE="0660", TAG+="uaccess"
RULES
    echo uinput > /etc/modules-load.d/60-lunaris.conf
    modprobe uinput 2>/dev/null || true
    if command -v udevadm >/dev/null; then
        udevadm control --reload-rules 2>/dev/null || true
        udevadm trigger --subsystem-match=misc --sysname-match=uinput 2>/dev/null || true
    fi
    # AppImages need FUSE 2.
    if ! { ldconfig -p 2>/dev/null | grep -q 'libfuse\.so\.2'; }; then
        info "Installing FUSE 2 for the AppImage"
        if command -v dnf >/dev/null; then dnf install -y fuse-libs || true
        elif command -v zypper >/dev/null; then zypper --non-interactive install libfuse2 || true
        elif command -v pacman >/dev/null; then pacman -S --noconfirm --needed fuse2 || true
        fi
        if ! { ldconfig -p 2>/dev/null | grep -q 'libfuse\.so\.2'; }; then
            echo "warning: libfuse.so.2 not found: install FUSE 2 (fuse2 / libfuse2 / fuse-libs), or the agent can't start at login" >&2
            export APPIMAGE_EXTRACT_AND_RUN=1
        fi
    fi
fi

# Unattended capture on Wayland: the root capture service hands the screen to
# the agent of the active desktop session (no portal prompt). The .deb
# installs and enables it itself; the AppImage gets the same unit here.
CAPTURE_SERVICE=0
if [ -d /run/systemd/system ] && command -v systemctl >/dev/null; then
    if [ "$AGENT_BIN" != /usr/bin/lunaris-agent ]; then
        info "Installing the capture service (lunaris-capture.service)"
        cat > /etc/systemd/system/lunaris-capture.service <<UNIT
# Lunaris capture service: lets the Lunaris agent in the active desktop
# session capture the screen without the Wayland portal prompt. Reads the
# screen only; no network, no input. Made by the Lunaris installer.
[Unit]
Description=Lunaris capture service (unattended screen capture)
After=systemd-logind.service

[Service]
Type=simple
# Unpacked to the private /tmp instead of a FUSE mount.
Environment=APPIMAGE_EXTRACT_AND_RUN=1
ExecStart=$AGENT_BIN --capture-service --allow-appimage $AGENT_BIN
Restart=on-failure
RestartSec=2
RuntimeDirectory=lunaris
RuntimeDirectoryMode=0755
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_PTRACE
NoNewPrivileges=yes
DevicePolicy=closed
DeviceAllow=char-drm rw
PrivateNetwork=yes
RestrictAddressFamilies=AF_UNIX
IPAddressDeny=any
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictNamespaces=yes
LockPersonality=yes
SystemCallArchitectures=native
UMask=0077

[Install]
WantedBy=multi-user.target
UNIT
        chmod 644 /etc/systemd/system/lunaris-capture.service
        systemctl daemon-reload || true
    fi
    if systemctl enable --now lunaris-capture.service >/dev/null 2>&1; then
        CAPTURE_SERVICE=1
    else
        echo "warning: the capture service did not start (systemctl status lunaris-capture); Wayland capture will use the portal prompt" >&2
    fi
fi

config="$TARGET_HOME/.config/lunaris-agent/agent_config.json"
args=(--enroll --server "$SERVER_URL" --config "$config")
[ -n "$NAME" ] && args+=(--name "$NAME")
[ -n "$CERT_SHA256" ] && args+=(--server-cert-sha256 "$CERT_SHA256")

info "Joining this computer to $SERVER_URL"
# The token goes in the environment, not on the agent's command line.
export LUNARIS_INSTALL_TOKEN="$TOKEN"
(cd "$TARGET_HOME" && runuser -u "$TARGET_USER" -- "$AGENT_BIN" "${args[@]}") \
    || die "the agent could not be set up (see above)"
unset LUNARIS_INSTALL_TOKEN

if [ "$CAPTURE_SERVICE" -eq 1 ]; then
    WAYLAND_NOTE="  * Wayland (GNOME, KDE, ...): the screen is captured through the
    lunaris-capture service, without asking on this screen (only for the
    user of the active desktop session; see systemctl status lunaris-capture)."
else
    WAYLAND_NOTE="  * Wayland (GNOME, KDE, ...): the first remote session asks on this screen
    which monitor to share. Someone has to accept it once; the choice is
    remembered (~/.config/lunaris/screencast-token)."
fi

cat <<EOF

Lunaris Agent $VERSION is installed for $TARGET_USER and joined to $SERVER_URL.

  * It starts when $TARGET_USER logs in to the desktop
    (~/.config/autostart/lunaris-agent.desktop). Log out and in again, or
    start it now in that desktop session: lunaris-agent --cli
  * The computer appears on the server once the agent runs; the install
    token is used then, after that the agent signs in with its own key.
$WAYLAND_NOTE
  * Remote keyboard/mouse works after the next login (/dev/uinput).
EOF

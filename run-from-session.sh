#!/usr/bin/env bash
# Run drm-plane-bench UNPRIVILEGED (and sandboxed with bwrap when available) from a
# terminal inside a running graphical session.
#
# sudo is used ONLY for:
#   - fgconsole / chvt (switching to a spare VT so the compositor drops DRM master, and back)
#   - vtconsole/*/bind (detaching/reattaching fbcon during the switch)
#   - setfacl (only if your user is not in the `video` group, because logind revokes
#     uaccess on /dev/dri/card* when switching to an empty VT; logind restores it on return)
#
# The drm-plane-bench binary itself NEVER runs as root. When `bwrap` (bubblewrap) is
# installed and --csv is not requested, it runs inside an empty --tmpfs / root with
# --unshare-all --clearenv (no $HOME, no $XDG_RUNTIME_DIR, no network, only /dev/dri,
# /proc, read-only /sys, and the static binary itself).
# Do not type while the spare VT is active.
#
#   ./run-from-session.sh                        # runs --suite (~42 s)
#   ./run-from-session.sh --suite --connector DP-1
#   ./run-from-session.sh --regression           # --suite + 5 regression cases (~77 s)
#   ./run-from-session.sh --dual-head [--connector-b DP-1]   # two CRTCs per commit (~28 s)
#   [SPARE_VT=8] [NO_BWRAP=1] ./run-from-session.sh --draw scene --seconds 30
set -euo pipefail

dir=$(dirname "$(readlink -f "$0")")
if [[ -z ${BIN:-} ]]; then
    for cand in \
        "$dir/drm-plane-bench" \
        "$dir/result/bin/drm-plane-bench" \
        "$dir/target/release/drm-plane-bench" \
        "$(command -v drm-plane-bench 2>/dev/null || true)"; do
        if [[ -n $cand && -x $cand ]]; then BIN=$cand; break; fi
    done
fi
[[ -n ${BIN:-} && -x $BIN ]] || { echo "drm-plane-bench binary not found; build it or set BIN=..." >&2; exit 1; }
BIN=$(readlink -f "$BIN")

find_cmd() {
    if [[ -x "/run/current-system/sw/bin/$1" ]]; then
        echo "/run/current-system/sw/bin/$1"
    else
        command -v "$1"
    fi
}

fgconsole_cmd=$(find_cmd fgconsole)
chvt_cmd=$(find_cmd chvt)
tee_cmd=$(find_cmd tee)

spare_vt=${SPARE_VT:-8}
vtcon=
for v in /sys/class/vtconsole/vtcon*; do
    if grep -q "frame buffer" "$v/name" 2>/dev/null; then vtcon=$v; break; fi
done

args=("$@")
if [[ ${#args[@]} -eq 0 ]]; then
    args=(--suite)
fi

want_csv=0
for a in "${args[@]}"; do
    [[ $a == "--csv" ]] && want_csv=1
done

cmd=("$BIN" "${args[@]}")
if [[ -z ${NO_BWRAP:-} && $want_csv -eq 0 ]] && command -v bwrap >/dev/null 2>&1; then
    cmd=(
        bwrap --unshare-all --clearenv --die-with-parent --new-session
        --tmpfs /
        --proc /proc
        --ro-bind /sys /sys
        --dev-bind /dev/dri /dev/dri
        --ro-bind "$BIN" /bench
        /bench "${args[@]}"
    )
fi

session_vt=$(sudo "$fgconsole_cmd")
[[ $session_vt != "$spare_vt" ]] || { echo "session is on VT$spare_vt; set SPARE_VT" >&2; exit 2; }

out=$(mktemp /tmp/drm-plane-bench-XXXXXX.txt)
restore() {
    sudo "$chvt_cmd" "$session_vt" || true
    [[ -z $vtcon ]] || echo 1 | sudo "$tee_cmd" "$vtcon/bind" >/dev/null || true
}
trap restore EXIT

[[ -z $vtcon ]] || echo 0 | sudo "$tee_cmd" "$vtcon/bind" >/dev/null
sudo "$chvt_cmd" "$spare_vt"
sleep 2

# Check whether we still have rw access to any /dev/dri/card* (true if in `video` group).
# If systemd-logind revoked uaccess on the empty VT, grant temporary rw so the binary
# still runs completely unprivileged (logind resets ACLs when we chvt back).
can_open_drm=0
for c in /dev/dri/card*; do
    if [[ -r $c && -w $c ]]; then can_open_drm=1; break; fi
done
if [[ $can_open_drm -eq 0 ]]; then
    if command -v setfacl >/dev/null 2>&1; then
        sudo "$(find_cmd setfacl)" -m "u:${USER}:rw" /dev/dri/card* 2>/dev/null || true
        timeout 600 "${cmd[@]}" >"$out" 2>&1 || true
    else
        card_grp=$(stat -c %G /dev/dri/card0 2>/dev/null || echo video)
        timeout 600 sudo -u "$USER" -g "$card_grp" -- "${cmd[@]}" >"$out" 2>&1 || true
    fi
else
    timeout 600 "${cmd[@]}" >"$out" 2>&1 || true
fi

restore
trap - EXIT
cat "$out"
echo ""
echo "Report saved to: $out"

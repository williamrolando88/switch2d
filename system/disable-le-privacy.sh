#!/usr/bin/env bash
# Disable the Bluetooth LE privacy policy flag on every adapter.
#
# WHY (measured A/B, round 3): with LE privacy enabled the kernel initiates LE
# connections from a resolvable private address (RPA). A Switch 2 controller
# that stored our *public* address in its SET_MAC pairing command sees an
# address it has never been bonded to and ignores the connection: 32/32 dial
# attempts failed (HCI status 0x3E -> ENOSYS on the public address type, timeout
# on the random one). With the policy flag off the same pad connected on the
# first round and streamed 3975 reports over 45 s with zero drops.
#
# Only the "privacy" policy flag matters here. "ll-privacy" (controller-side
# link-layer privacy) may stay enabled; it does not change the initiator address
# the pad sees.
#
# This is a host-wide setting: while it is off, the kernel uses the adapter's
# public LE address toward *every* LE device, not just controllers. See
# system/switch2d-disable-le-privacy.service for what that trade-off means.
#
# The script is idempotent: if privacy is already off it does NOT power-cycle
# the adapter (that would drop live controller links).
set -euo pipefail

log() { printf '[disable-le-privacy] %s\n' "$*"; }

# One "current settings:" line from `btmgmt info` (empty when unreadable).
current_settings() {
    btmgmt -i "$1" info 2>&1 \
        | sed -n 's/.*current settings:[[:space:]]*//p' \
        | head -n 1 \
        || true
}

adapter_count=0
# Non-zero when an adapter that needed the fix still reports privacy afterwards.
status=0
for sysfs in /sys/class/bluetooth/hci*; do
    [ -e "$sysfs" ] || continue
    adapter_count=$((adapter_count + 1))
    index="${sysfs##*hci}"

    # At boot this unit starts while bluetoothd may still be powering the
    # adapter; btmgmt would report "Powered: off" and privacy would be skipped.
    powered=""
    for _ in {1..60}; do
        if busctl get-property org.bluez "/org/bluez/hci${index}" \
            org.bluez.Adapter1 Powered 2>/dev/null | grep -q 'true'; then
            powered=1
            break
        fi
        sleep 0.5
    done
    if [ -z "$powered" ]; then
        log "hci${index}: adapter not powered after 30 s, skipping"
        continue
    fi

    current=$(current_settings "$index")
    if [ -z "$current" ]; then
        log "hci${index}: could not read current settings from btmgmt (needs root), skipping"
        continue
    fi
    log "hci${index}: current settings: ${current}"

    # Exact token match: "privacy" is the policy flag we need off.
    # "ll-privacy" (also a token in newer kernels) is deliberately not matched.
    case " ${current} " in
        *" privacy "*)
            log "hci${index}: privacy is ON, cycling power to turn it off"
            ;;
        *)
            log "hci${index}: privacy already off, leaving power state untouched"
            continue
            ;;
    esac

    # btmgmt can exit 0 even when the kernel rejects a mgmt command, so judge
    # success by re-reading the settings, not by exit status alone.
    btmgmt -i "$index" power off || log "hci${index}: 'power off' reported failure"
    btmgmt -i "$index" privacy off || log "hci${index}: 'privacy off' reported failure"
    btmgmt -i "$index" power on || log "hci${index}: 'power on' reported failure"

    updated=$(current_settings "$index")
    log "hci${index}: settings after: ${updated:-<unreadable>}"
    case " ${updated} " in
        *" privacy "*)
            log "hci${index}: privacy is STILL on (not running as root?)"
            status=1
            ;;
        *)
            log "hci${index}: privacy is now off"
            ;;
    esac
done

if [ "$adapter_count" -eq 0 ]; then
    log "no Bluetooth adapters found, nothing to do"
fi
log "done"
exit "$status"

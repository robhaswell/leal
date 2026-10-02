#!/bin/bash
# Attaches disk images for the hosted RemovableDriveTests (app/AppTests),
# from outside the App Sandbox (cons-12, phase 1 review).
#
# The hosted tests run inside the sandboxed Leal.app, where `hdiutil create`
# and `hdiutil attach` fail with "Device not configured" (`diskutil image
# attach` with "Failed to attach disk image", and `diskutil` can't see the
# disk at all). `hdiutil detach -force` and `hdiutil info` do work there.
# So the Leal scheme's test pre-action starts this helper (`start`), and the
# post-action stops it (`stop`). Meanwhile it serves requests the test
# writes into the app container's temporary folder:
#
#   leal-disk-images/<id>/source/   the files to put on the volume
#   leal-disk-images/<id>/request   "fs=ExFAT format=UDBZ size=64m"
#
# It makes the image from the source folder, attaches it (hidden from
# Finder, not in /Volumes) at leal-disk-images/<id>/volume, inside the
# container, where the sandboxed host may read and write, and writes
# `ready` ("ok /dev/diskN", or "failed" and hdiutil's output). When the
# test writes `detach`, the helper pulls the drive at once (`hdiutil
# detach -force`, which takes about 10 s inside the sandbox while a file
# on the volume is open) and writes `detached`. At the end the test writes
# `done`, and the helper detaches whatever is left and removes the folder.
#
# The helper always detaches every image it attached: on `done`, on
# `stop`, on SIGTERM and SIGINT, and when it gives up after an hour.
# Several helpers may run at once (several runs of the tests); each request
# is claimed by one, by renaming it. Never kills anything by name.

set -uo pipefail

container_tmp="$HOME/Library/Containers/io.github.robhaswell.leal/Data/tmp"
work="$container_tmp/leal-disk-images"
script="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
lifetime=3600
attempts=5
transient='Resource busy|Resource temporarily unavailable|no mountable file systems|Device not configured'

# The run's own files (pid and log), next to the build products.
state_dir() {
    local dir="${LEAL_DISK_IMAGE_STATE:-${PROJECT_DIR:-$PWD}/../build}"
    mkdir -p "$dir"
    echo "$dir"
}

log() { echo "$(date '+%H:%M:%S') disk-image-helper[$$]: $*"; }

# hdiutil with retries on its transient errors, as the Rust tests' DiskImage.
hdiutil_retrying() {
    local output status backoff=0.25
    for attempt in $(seq "$attempts"); do
        output="$(/usr/bin/hdiutil "$@" 2>&1)"
        status=$?
        if [ "$status" -eq 0 ]; then
            echo "$output"
            return 0
        fi
        if ! grep -Eq "$transient" <<<"$output" || [ "$attempt" -eq "$attempts" ]; then
            echo "hdiutil $1 failed on attempt $attempt of $attempts (exit $status): $output"
            return 1
        fi
        sleep "$backoff"
        backoff="$(echo "$backoff * 2" | bc)"
    done
}

# The whole-disk devices attached from image $1, from `hdiutil info -plist`.
devices_of() {
    /usr/bin/hdiutil info -plist 2>/dev/null | /usr/bin/python3 -c '
import plistlib, sys, os
image = os.path.realpath(sys.argv[1])
for entry in plistlib.loads(sys.stdin.buffer.read()).get("images", []):
    path = entry.get("image-path", "")
    if path == sys.argv[1] or os.path.realpath(path) == image:
        devices = [e["dev-entry"] for e in entry.get("system-entities", []) if "dev-entry" in e]
        if devices:
            print(min(devices, key=len))
' "$1"
}

# Detaches every device attached from image $1: normally, then by force.
detach_all() {
    local image="$1" devices
    for force in "" "" -force -force; do
        devices="$(devices_of "$image")"
        [ -z "$devices" ] && return 0
        for device in $devices; do
            /usr/bin/hdiutil detach "$device" $force > /dev/null 2>&1
        done
        sleep 0.25
    done
    devices="$(devices_of "$image")"
    if [ -n "$devices" ]; then
        log "warning: couldn't detach $image ($devices); run \`hdiutil detach -force\` on it"
        return 1
    fi
}

# Serves one claimed request in folder $1.
serve() {
    local folder="$1" id fs=ExFAT format=UDRW size="" result
    id="$(basename "$folder")"
    local pairs=()
    read -r -a pairs < "$folder/claimed"
    for pair in "${pairs[@]}"; do
        case "$pair" in
            fs=*) fs="${pair#fs=}" ;;
            format=*) format="${pair#format=}" ;;
            size=*) size="${pair#size=}" ;;
        esac
    done
    local image="$folder/volume.dmg" mount="$folder/volume"
    mkdir -p "$mount"
    log "request $id: $fs $format ${size:-(fitted)}"
    local create=(create -quiet -srcfolder "$folder/source" -fs "$fs" -format "$format" -volname "$id")
    [ -n "$size" ] && create+=(-size "$size")
    if result="$(rm -f "$image"; hdiutil_retrying "${create[@]}" "$image")" \
        && result="$(detach_all "$image"; hdiutil_retrying attach -nobrowse -noverify -noautoopen -mountpoint "$mount" "$image")"; then
        echo "$image" >> "$attached"
        printf 'ok %s\n' "$(devices_of "$image")" > "$folder/ready.tmp"
        log "request $id: attached at $mount"
    else
        printf 'failed\n%s\n' "$result" > "$folder/ready.tmp"
        log "request $id: $result"
        detach_all "$image"
    fi
    mv "$folder/ready.tmp" "$folder/ready"
}

# Pulls the drive of the request in folder $1 out, as if unplugged.
pull() {
    local folder="$1" device result
    device="$(devices_of "$folder/volume.dmg")"
    result="$(/usr/bin/hdiutil detach "$device" -force 2>&1)"
    printf '%s\n' "$result" > "$folder/detached.tmp"
    mv "$folder/detached.tmp" "$folder/detached"
    log "request $(basename "$folder"): pulled out ($result)"
}

# Detaches the image of a finished request in folder $1 and removes it.
finish() {
    local folder="$1"
    detach_all "$folder/volume.dmg"
    rm -rf "$folder"
    log "request $(basename "$folder"): done"
}

cleanup() {
    if [ -s "$attached" ]; then
        while read -r image; do
            [ -e "$image" ] && detach_all "$image"
            rm -rf "$(dirname "$image")"
        done < "$attached"
    fi
    rm -f "$attached" "$work/.helper-$$"
    log "stopped"
}

watch() {
    attached="$(mktemp -t leal-disk-images)"
    trap cleanup EXIT
    trap 'exit 0' TERM INT
    log "watching $work"
    local deadline=$((SECONDS + lifetime)) stop="${1:-}"
    while [ "$SECONDS" -lt "$deadline" ]; do
        [ -n "$stop" ] && [ -e "$stop" ] && break
        # The container exists once the test host has run; never make it.
        if [ -d "$container_tmp" ]; then
            mkdir -p "$work"
            touch "$work/.helper-$$"
            for request in "$work"/*/request; do
                [ -e "$request" ] || continue
                # Claim it; another helper may have got there first.
                mv "$request" "$(dirname "$request")/claimed" 2> /dev/null && serve "$(dirname "$request")"
            done
            for detach in "$work"/*/detach; do
                [ -e "$detach" ] || continue
                local folder
                folder="$(dirname "$detach")"
                grep -qxF "$folder/volume.dmg" "$attached" || continue
                rm -f "$detach"
                pull "$folder"
            done
            for done_file in "$work"/*/done; do
                [ -e "$done_file" ] && grep -qxF "$(dirname "$done_file")/volume.dmg" "$attached" && finish "$(dirname "$done_file")"
            done
        fi
        sleep 0.2
    done
}

case "${1:-}" in
    start)
        dir="$(state_dir)"
        rm -f "$dir/disk-image-helper.stop"
        # A clean environment: hdiutil complains about Xcode's variables.
        env -i HOME="$HOME" PATH=/usr/bin:/bin:/usr/sbin:/sbin TMPDIR="${TMPDIR:-/tmp}" \
            nohup "$script" watch "$dir/disk-image-helper.stop" >> "$dir/disk-image-helper.log" 2>&1 < /dev/null &
        echo $! > "$dir/disk-image-helper.pid"
        ;;
    stop)
        dir="$(state_dir)"
        touch "$dir/disk-image-helper.stop"
        if [ -s "$dir/disk-image-helper.pid" ]; then
            pid="$(cat "$dir/disk-image-helper.pid")"
            # Only the helper this run started, by its PID.
            for _ in $(seq 50); do
                kill -0 "$pid" 2> /dev/null || break
                sleep 0.2
            done
            kill "$pid" 2> /dev/null
            rm -f "$dir/disk-image-helper.pid"
        fi
        ;;
    watch)
        watch "${2:-}"
        ;;
    *)
        echo "usage: $0 start|stop|watch [stop-file]" >&2
        exit 2
        ;;
esac

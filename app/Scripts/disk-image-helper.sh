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
# writes into the app container's temporary folder, in a folder of this
# checkout's own (`checkout_key`: the scheme passes the same path to the
# test as LEAL_DISK_IMAGE_CHECKOUT), so helpers of other worktrees never
# touch them:
#
#   leal-disk-images/<checkout>/<id>/source/   the files to put on the volume
#   leal-disk-images/<checkout>/<id>/request   "fs=ExFAT format=UDBZ size=64m"
#
# It makes the image from the source folder, attaches it (hidden from
# Finder, not in /Volumes) at <id>/volume, inside the container, where the
# sandboxed host may read and write, and writes `ready` ("ok /dev/diskN",
# or "failed" and hdiutil's output). When the test writes `detach`, the
# helper pulls the drive at once (`hdiutil detach -force`, which takes about
# 10 s inside the sandbox while a file on the volume is open) and writes
# `detached`. At the end the test writes `done`, and the helper detaches
# whatever is left and removes the folder.
#
# Each run of the tests has its own helper, pid and stop file, keyed by the
# xcodebuild process that ran the pre-action (its parent). The helper
# detaches every image it attached: on `done`, on `stop`, on SIGTERM and
# SIGINT, as soon as that xcodebuild has gone (an interrupted run), and after
# 20 minutes without a request (the time runs from the last request, not from
# the start: the pre-action runs before xcodebuild compiles, and a slow build
# must not use it up). `start` also detaches what an earlier helper of this
# checkout left behind, if that helper is gone. Never kills anything by name.

set -uo pipefail

container_tmp="$HOME/Library/Containers/io.github.robhaswell.leal/Data/tmp"
script="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
# How long the helper waits for the next request, once it has had one.
idle_timeout=1200
# Before the first request: long enough for a slow build, but bounded, so a
# run that never reaches the tests (a failed build, or Xcode as the owner)
# can't leave the helper running.
first_request_timeout=7200
attempts=5
transient='Resource busy|Resource temporarily unavailable|no mountable file systems|Device not configured'

# This checkout's key: the first 12 hex digits of the SHA-256 of
# PROJECT_DIR (the app/ folder). The tests compute the same.
checkout_key() {
    printf '%s' "${PROJECT_DIR:?PROJECT_DIR is set by the scheme action}" | /usr/bin/shasum -a 256 | cut -c 1-12
}

# The xcodebuild (or Xcode) that runs the scheme's actions: the first
# ancestor by that name. The pre- and post-action find the same one.
owner_pid() {
    local pid="$PPID" name
    for _ in $(seq 12); do
        [ -n "$pid" ] && [ "$pid" -gt 1 ] || break
        name="$(ps -o comm= -p "$pid" 2> /dev/null)"
        case "$name" in
            *xcodebuild | */Xcode) echo "$pid"; return ;;
        esac
        pid="$(ps -o ppid= -p "$pid" 2> /dev/null | tr -d ' ')"
    done
}

# The run's own files (pids, stop files and the log), next to the build
# products.
state_dir() {
    local dir="${PROJECT_DIR:-$PWD}/../build/disk-image-helper"
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
            local why="not a transient error, so not retried"
            grep -Eq "$transient" <<<"$output" && why="still failing after the last attempt"
            # What hdiutil said (stdout and stderr), without its deprecation
            # warnings, on one line.
            local said
            said="$(grep -v 'is deprecated' <<<"$output" | tr '\n' ' ')"
            said="${said% }"
            echo "hdiutil $1 failed on attempt $attempt of $attempts ($why), exit $status: ${said:-(no output)}"
            return 1
        fi
        sleep "$backoff"
        backoff="$(echo "$backoff * 2" | bc)"
    done
}

# For each attachment of image $1 in `hdiutil info -plist`, its first
# whole-disk device (/dev/diskN: the disk, then any APFS container on it)
# still in /dev; detaching it detaches the rest. A device whose node has
# gone counts as detached even while `hdiutil info` still lists it:
# hdiutil lags behind the kernel for a moment after a detach, and then
# says "No such file or directory" to a detach of it.
devices_of() {
    /usr/bin/hdiutil info -plist 2>/dev/null | /usr/bin/python3 -c '
import plistlib, re, sys, os
image = os.path.realpath(sys.argv[1])
for entry in plistlib.loads(sys.stdin.buffer.read()).get("images", []):
    path = entry.get("image-path", "")
    if path == sys.argv[1] or os.path.realpath(path) == image:
        devices = [e["dev-entry"] for e in entry.get("system-entities", []) if "dev-entry" in e]
        present = [d for d in devices if re.fullmatch(r"/dev/disk[0-9]+", d) and os.path.exists(d)]
        if present:
            print(present[0])
' "$1"
}

# Detaches every device attached from image $1, as the Rust tests'
# DiskImage::detach_all: an ordinary detach, tried again with a backoff
# for 4 s while the volume is busy, then -force for 6 s more. Logs what
# hdiutil said if anything went wrong.
detach_all() {
    local image="$1" devices device force output status backoff=0.25 trouble=""
    local started=$SECONDS
    while :; do
        devices="$(devices_of "$image")"
        if [ -z "$devices" ]; then
            [ -n "$trouble" ] && log "detached $image after:$trouble"
            return 0
        fi
        [ $((SECONDS - started)) -ge 10 ] && break
        force=""
        [ $((SECONDS - started)) -ge 4 ] && force=-force
        for device in $devices; do
            output="$(/usr/bin/hdiutil detach "$device" $force 2>&1)"
            status=$?
            [ "$status" -ne 0 ] && trouble="$trouble
  at $((SECONDS - started)) s, hdiutil detach $device${force:+ $force} failed (exit $status): $(grep -v 'is deprecated' <<<"$output")"
        done
        [ -z "$(devices_of "$image")" ] && continue
        sleep "$backoff"
        case "$backoff" in 0.25) backoff=0.5 ;; *) backoff=1 ;; esac
    done
    log "warning: couldn't detach $image ($devices); run \`hdiutil detach -force\` on it:$trouble"
    return 1
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
    echo $$ > "$folder/helper"
    mkdir -p "$mount"
    log "request $id: $fs $format ${size:-(fitted)}"
    # Not `-quiet`: it silences hdiutil's errors too, so a failure said
    # nothing, and the retry above, which looks for a transient error in
    # what hdiutil said, never saw one (CI run 37444310738).
    local create=(create -srcfolder "$folder/source" -fs "$fs" -format "$format" -volname "$id")
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
    device="$(cut -d ' ' -f 2 < "$folder/ready")"
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

# Detaches what helpers of this checkout that are gone left attached (a
# run interrupted before its helper could clean up), and removes their
# folders. A live helper's requests are left alone.
sweep() {
    local folder owner image
    for folder in "$work"/*/; do
        [ -d "$folder" ] || continue
        folder="${folder%/}"
        owner="$(cat "$folder/helper" 2> /dev/null)"
        if [ -n "$owner" ] && kill -0 "$owner" 2> /dev/null; then
            continue
        fi
        # Not claimed yet: a test may be about to ask; leave it to the
        # helper that claims it. Unless it is old (a test that has gone).
        if [ -z "$owner" ] && [ -z "$(find "$folder" -maxdepth 0 -mmin +20 2> /dev/null)" ]; then
            continue
        fi
        image="$folder/volume.dmg"
        detach_all "$image"
        rm -rf "$folder"
        log "swept $(basename "$folder") (its helper ${owner:-none} is gone)"
    done
    for beat in "$work"/.helper-*; do
        [ -e "$beat" ] || continue
        kill -0 "${beat##*.helper-}" 2> /dev/null || rm -f "$beat"
    done
}

watch() {
    local owner="$1" stop="$2"
    attached="$(mktemp -t leal-disk-images)"
    trap cleanup EXIT
    trap 'exit 0' TERM INT
    log "watching $work for xcodebuild $owner"
    # The idle time runs from the last request (or detach, or done). With
    # no xcodebuild to watch (`owner` is none), it runs from the start until
    # the first request, so the helper can't outlive a run it can't see.
    local last_request="" watched=yes started=$SECONDS
    [ "$owner" = none ] && watched=no
    while :; do
        [ -e "$stop" ] && break
        if [ -n "$last_request" ]; then
            [ $((SECONDS - last_request)) -ge "$idle_timeout" ] && { log "idle for $idle_timeout s"; break; }
        elif [ "$watched" = no ] && [ $((SECONDS - started)) -ge "$idle_timeout" ]; then
            log "no request for $idle_timeout s"
            break
        elif [ $((SECONDS - started)) -ge "$first_request_timeout" ]; then
            log "no request for $first_request_timeout s"
            break
        fi
        # The run was interrupted: its post-action will never come.
        if [ "$watched" = yes ] && ! kill -0 "$owner" 2> /dev/null; then
            log "xcodebuild $owner has gone"
            break
        fi
        # The container exists once the test host has run; never make it.
        if [ -d "$container_tmp" ]; then
            mkdir -p "$work"
            touch "$work/.helper-$$"
            for request in "$work"/*/request; do
                [ -e "$request" ] || continue
                # Claim it; another helper of this checkout may have got
                # there first.
                if mv "$request" "$(dirname "$request")/claimed" 2> /dev/null; then
                    serve "$(dirname "$request")"
                    last_request=$SECONDS
                fi
            done
            for detach in "$work"/*/detach; do
                [ -e "$detach" ] || continue
                local folder
                folder="$(dirname "$detach")"
                grep -qxF "$folder/volume.dmg" "$attached" || continue
                rm -f "$detach"
                pull "$folder"
                last_request=$SECONDS
            done
            for done_file in "$work"/*/done; do
                [ -e "$done_file" ] && grep -qxF "$(dirname "$done_file")/volume.dmg" "$attached" && { finish "$(dirname "$done_file")"; last_request=$SECONDS; }
            done
        fi
        sleep 0.05
    done
}

case "${1:-}" in
    start)
        dir="$(state_dir)"
        work="$container_tmp/leal-disk-images/$(checkout_key)"
        # The xcodebuild that runs this action (and, later, the post-action),
        # whose end ends the helper; "none" if there is none, when only the
        # stop file and the idle timeout do.
        owner="$(owner_pid)"
        owner="${owner:-none}"
        sweep >> "$dir/helper.log" 2>&1
        rm -f "$dir/$owner.stop"
        # A clean environment: hdiutil complains about Xcode's variables.
        env -i HOME="$HOME" PATH=/usr/bin:/bin:/usr/sbin:/sbin TMPDIR="${TMPDIR:-/tmp}" \
            nohup "$script" watch "$work" "$owner" "$dir/$owner.stop" >> "$dir/helper.log" 2>&1 < /dev/null &
        echo $! > "$dir/$owner.pid"
        ;;
    stop)
        dir="$(state_dir)"
        owner="$(owner_pid)"
        owner="${owner:-none}"
        touch "$dir/$owner.stop"
        if [ -s "$dir/$owner.pid" ]; then
            pid="$(cat "$dir/$owner.pid")"
            # Only the helper this run started, by its PID.
            for _ in $(seq 50); do
                kill -0 "$pid" 2> /dev/null || break
                sleep 0.2
            done
            kill "$pid" 2> /dev/null
        fi
        rm -f "$dir/$owner.pid" "$dir/$owner.stop"
        ;;
    watch)
        work="$2"
        watch "$3" "$4"
        ;;
    *)
        echo "usage: $0 start|stop  (from the Leal scheme's test actions)" >&2
        exit 2
        ;;
esac

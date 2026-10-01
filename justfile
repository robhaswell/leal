# Leal task runner. Run `just` to list the recipes.

# The two slices of the universal static library (DESIGN §6).
ffi_targets := "aarch64-apple-darwin x86_64-apple-darwin"
# Matches MACOSX_DEPLOYMENT_TARGET in app/project.yml.
macos_deployment_target := "14.0"
# Xcode's build products, kept inside the repo (and git-ignored) so builds
# don't depend on ~/Library/Developer/Xcode/DerivedData.
derived_data := "build/DerivedData"

# List the recipes.
default:
    @just --list

# `--all-features` (here and in `test`, `test-deep` and `lint`) also covers
# leal-ffi's test-only exports (the `test-exports` feature; see `ffi`).

# Format check, clippy (-D warnings), tests, doctests and rustdoc (-D warnings). Must pass before every commit.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo nextest run --workspace --all-features
    cargo test --workspace --doc
    just doc

# Build the API docs, failing on any rustdoc warning (such as a broken intra-doc link).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --quiet

# `check`, then the app's XCTest suite. Must pass before a task's final commit.
check-all: check app-test

# Run all tests. Nextest does not run doctests, so they run separately.
test:
    cargo nextest run --workspace --all-features
    cargo test --workspace --doc

# Run all tests with many more property-test cases (the default is 256 per test).
test-deep cases="20000":
    PROPTEST_CASES={{cases}} cargo nextest run --workspace --all-features

# Format all code in place.
fmt:
    cargo fmt --all

# Run clippy on all crates and targets, with warnings as errors.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run the benchmarks.
bench:
    cargo bench --workspace

# Build the universal libleal_ffi.a and generate the Swift bindings (profile: debug or release).
ffi profile="debug" test_exports="auto":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ profile }}" in
        debug) release_flag="" ;;
        release) release_flag="--release" ;;
        *) echo "error: profile must be debug or release, not '{{ profile }}'" >&2; exit 1 ;;
    esac
    # test_exports=on builds leal-ffi with its `test-exports` feature: exports
    # that only the XCTest suite calls, such as `debug_panic`. `auto` means on
    # for debug and off for release, so the release library that `just app
    # release` (and an IDE Release build) links never has them. Only
    # `app-test release` builds a release library with them.
    test_exports="{{ test_exports }}"
    if [ "$test_exports" = auto ]; then
        if [ "{{ profile }}" = debug ]; then test_exports=on; else test_exports=off; fi
    fi
    case "$test_exports" in
        on) features="--features test-exports" ;;
        off) features="" ;;
        *) echo "error: test_exports must be auto, on or off, not '$test_exports'" >&2; exit 1 ;;
    esac

    # 1. One static library per architecture. Cargo skips targets that are
    #    already up to date.
    export MACOSX_DEPLOYMENT_TARGET={{ macos_deployment_target }}
    slices=""
    for target in {{ ffi_targets }}; do
        cargo build --quiet --package leal-ffi --lib --target "$target" $release_flag $features
        slices="$slices target/$target/{{ profile }}/libleal_ffi.a"
    done

    # 2. Combine them into one universal library. Skip this if the slices
    #    are the ones it was last made from, so an unchanged library doesn't
    #    make Xcode relink. "The same slices" is recorded in a stamp file:
    #    each slice's modification time and size, and test_exports. Comparing
    #    modification times with `-nt` isn't enough: Cargo copies a cached
    #    build into place with its original, older time, so switching
    #    test_exports back to a variant built earlier gives slices that are
    #    *older* than the universal library, though different.
    universal="target/universal/{{ profile }}/libleal_ffi.a"
    stamp="target/universal/{{ profile }}.slices"
    mkdir -p "$(dirname "$universal")"
    current="test_exports=$test_exports $(stat -f '%Fm %z' $slices | tr '\n' ' ')"
    if [ ! -e "$universal" ] || [ "$(cat "$stamp" 2>/dev/null)" != "$current" ]; then
        lipo -create $slices -output "$universal"
        echo "$current" > "$stamp"
    fi

    # 3. Swift bindings, read from the library's embedded metadata ("library
    #    mode"). Both slices have the same metadata, so read the first. Write
    #    to a scratch directory and copy only files that changed, so
    #    unchanged bindings don't make Xcode recompile Swift.
    scratch="target/uniffi-swift/{{ profile }}"
    rm -rf "$scratch"
    cargo run --quiet --package uniffi-bindgen -- generate --no-format \
        --library "target/aarch64-apple-darwin/{{ profile }}/libleal_ffi.a" \
        --language swift --out-dir "$scratch"
    mkdir -p app/Generated
    for file in "$scratch"/*; do
        cmp -s "$file" "app/Generated/$(basename "$file")" || cp "$file" app/Generated/
    done
    echo "ffi: $universal and app/Generated/ are up to date"

# Generate app/Leal.xcodeproj from app/project.yml.
xcodeproj:
    xcodegen generate --quiet --spec app/project.yml

# Build Leal.app (profile: debug or release). Fails on any compiler or linker warning.
app profile="debug": (ffi profile) xcodeproj
    #!/usr/bin/env bash
    set -euo pipefail
    configuration="$(just _configuration {{ profile }})"
    mkdir -p build
    log="build/xcodebuild-build.log"
    # LEAL_FFI_PREBUILT=1: `ffi` already ran, so the project's RustFFI
    # phase (for IDE builds) skips it instead of running it again. It is an
    # environment variable, not a build setting, so builds from `just` and
    # from the IDE have identical settings and don't relink each other.
    LEAL_FFI_PREBUILT=1 xcodebuild -quiet -project app/Leal.xcodeproj -scheme Leal \
        -configuration "$configuration" -derivedDataPath {{ derived_data }} \
        -destination "platform=macOS,arch=$(uname -m)" \
        build 2>&1 | tee "$log"
    just _no_warnings "$log"
    echo "app: {{ derived_data }}/Build/Products/$configuration/Leal.app"

# Build and launch Leal.app (profile: debug or release).
run profile="debug": (app profile)
    #!/usr/bin/env bash
    set -euo pipefail
    # `open` only brings an already-running Leal to the front, which would
    # show the old build. Quit it first and wait until it has exited.
    if pkill -x Leal; then
        for _ in $(seq 50); do
            pgrep -x Leal > /dev/null || break
            sleep 0.1
        done
        if pgrep -x Leal > /dev/null; then
            echo "error: the running Leal didn't quit; quit it and try again" >&2
            exit 1
        fi
    fi
    open "{{ derived_data }}/Build/Products/$(just _configuration {{ profile }})/Leal.app"

# Run the app's XCTest suite, which calls Rust through the Swift bindings (profile: debug or release).
app-test profile="debug": (ffi profile "on") xcodeproj
    #!/usr/bin/env bash
    set -euo pipefail
    # `release` runs the same tests against the Rust release profile and the
    # Release configuration, so the optimised build is tested too (CI runs
    # both). Its library includes the test-only exports; the next `just app
    # release` rebuilds it without them.
    configuration="$(just _configuration {{ profile }})"
    mkdir -p build
    log="build/xcodebuild-test.log"
    results="build/LealTests.xcresult"
    rm -rf "$results"
    status=0
    # LEAL_FFI_PREBUILT=1: see `app`.
    LEAL_FFI_PREBUILT=1 xcodebuild -quiet -project app/Leal.xcodeproj -scheme Leal \
        -configuration "$configuration" -derivedDataPath {{ derived_data }} \
        -destination "platform=macOS,arch=$(uname -m)" \
        -resultBundlePath "$results" \
        test 2>&1 | tee "$log" || status=$?

    # `-quiet` hides test results, so read them from the result bundle.
    summary="$(xcrun xcresulttool get test-results summary --path "$results" --compact 2>/dev/null || true)"
    count() { grep -o "\"$1\":[0-9]*" <<<"$summary" | head -n 1 | cut -d: -f2; }
    echo "app-test ($configuration): $(count passedTests) passed, $(count failedTests) failed, $(count skippedTests) skipped"
    if [ "$status" -ne 0 ]; then
        xcrun xcresulttool get test-results summary --path "$results" || true
        exit "$status"
    fi
    just _no_warnings "$log"

# Build the app the way the Xcode IDE does (⌘B), from a fresh clone's state. CI runs it.
ide-build:
    #!/usr/bin/env bash
    set -euo pipefail
    # Every other recipe runs `just ffi` itself and sets LEAL_FFI_PREBUILT, so
    # the RustFFI target skips. Here xcodebuild runs with neither, and with a
    # minimal PATH, so the RustFFI target's script does the whole job.
    # Generate the project with no bindings yet, as in a fresh clone.
    rm -rf app/Generated
    just xcodeproj
    mkdir -p build
    log="build/xcodebuild-ide.log"
    # `env -i`: no LEAL_FFI_PREBUILT, and no Homebrew or rustup on PATH, as
    # in Xcode's own script environment.
    env -i HOME="$HOME" PATH=/usr/bin:/bin:/usr/sbin:/sbin \
        xcodebuild -quiet -project app/Leal.xcodeproj -scheme Leal \
        -configuration Debug -derivedDataPath {{ derived_data }} \
        -destination "platform=macOS,arch=$(uname -m)" \
        build 2>&1 | tee "$log"
    for file in leal_ffi.swift leal_ffiFFI.h leal_ffiFFI.modulemap; do
        if [ ! -s "app/Generated/$file" ]; then
            echo "error: the RustFFI target didn't generate app/Generated/$file" >&2
            exit 1
        fi
    done
    just _no_warnings "$log"
    echo "ide-build: the RustFFI target built the library and bindings, and the app built"

# The Xcode configuration for a Cargo profile.
[private]
_configuration profile:
    #!/usr/bin/env bash
    case "{{ profile }}" in
        debug) echo Debug ;;
        release) echo Release ;;
        *) echo "error: profile must be debug or release, not '{{ profile }}'" >&2; exit 1 ;;
    esac

# Fail if an xcodebuild log has warnings. `-quiet` prints only warnings and
# errors, so this catches linker and build-system warnings as well as Swift's.
[private]
_no_warnings log:
    #!/usr/bin/env bash
    if grep -qi "warning:" "{{ log }}"; then
        echo "error: the build has warnings (see {{ log }}); Leal builds with zero warnings" >&2
        exit 1
    fi

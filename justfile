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

# Format check, clippy (-D warnings), tests and doctests. Must pass before every commit.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo nextest run --workspace
    cargo test --workspace --doc

# `check`, then the app's XCTest suite. Must pass before a task's final commit.
check-all: check app-test

# Run all tests. Nextest does not run doctests, so they run separately.
test:
    cargo nextest run --workspace
    cargo test --workspace --doc

# Format all code in place.
fmt:
    cargo fmt --all

# Run clippy on all crates and targets, with warnings as errors.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Run the benchmarks.
bench:
    cargo bench --workspace

# Build the universal libleal_ffi.a and generate the Swift bindings (profile: debug or release).
ffi profile="debug":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ profile }}" in
        debug) release_flag="" ;;
        release) release_flag="--release" ;;
        *) echo "error: profile must be debug or release, not '{{ profile }}'" >&2; exit 1 ;;
    esac

    # 1. One static library per architecture. Cargo skips targets that are
    #    already up to date.
    export MACOSX_DEPLOYMENT_TARGET={{ macos_deployment_target }}
    slices=""
    for target in {{ ffi_targets }}; do
        cargo build --quiet --package leal-ffi --lib --target "$target" $release_flag
        slices="$slices target/$target/{{ profile }}/libleal_ffi.a"
    done

    # 2. Combine them into one universal library. Skip this when it is newer
    #    than both slices, so an unchanged library doesn't make Xcode relink.
    universal="target/universal/{{ profile }}/libleal_ffi.a"
    mkdir -p "$(dirname "$universal")"
    stale=false
    for slice in $slices; do
        [ "$universal" -nt "$slice" ] || stale=true
    done
    if $stale; then
        lipo -create $slices -output "$universal"
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
    xcodebuild -quiet -project app/Leal.xcodeproj -scheme Leal \
        -configuration "$configuration" -derivedDataPath {{ derived_data }} \
        -destination "platform=macOS,arch=$(uname -m)" \
        build 2>&1 | tee "$log"
    just _no_warnings "$log"
    echo "app: {{ derived_data }}/Build/Products/$configuration/Leal.app"

# Build and launch Leal.app (profile: debug or release).
run profile="debug": (app profile)
    open "{{ derived_data }}/Build/Products/$(just _configuration {{ profile }})/Leal.app"

# Run the app's XCTest suite, which calls Rust through the Swift bindings.
app-test: (ffi "debug") xcodeproj
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p build
    log="build/xcodebuild-test.log"
    results="build/LealTests.xcresult"
    rm -rf "$results"
    status=0
    xcodebuild -quiet -project app/Leal.xcodeproj -scheme Leal \
        -configuration Debug -derivedDataPath {{ derived_data }} \
        -destination "platform=macOS,arch=$(uname -m)" \
        -resultBundlePath "$results" \
        test 2>&1 | tee "$log" || status=$?

    # `-quiet` hides test results, so read them from the result bundle.
    summary="$(xcrun xcresulttool get test-results summary --path "$results" --compact 2>/dev/null || true)"
    count() { grep -o "\"$1\":[0-9]*" <<<"$summary" | head -n 1 | cut -d: -f2; }
    echo "app-test: $(count passedTests) passed, $(count failedTests) failed, $(count skippedTests) skipped"
    if [ "$status" -ne 0 ]; then
        xcrun xcresulttool get test-results summary --path "$results" || true
        exit "$status"
    fi
    just _no_warnings "$log"

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

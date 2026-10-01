# Leal task runner. Run `just` to list the recipes.

# The two slices of the universal static library (DESIGN §6).
ffi_targets := "aarch64-apple-darwin x86_64-apple-darwin"
# Matches MACOSX_DEPLOYMENT_TARGET in app/project.yml.
macos_deployment_target := "14.0"
# Xcode's build products, kept inside the repo (and git-ignored) so builds
# don't depend on ~/Library/Developer/Xcode/DerivedData.
derived_data := "build/DerivedData"
# `app-test`'s own build products. `app-test release` links a release library
# with leal-ffi's test-only exports, so its Leal.app must not land where
# `just app release` puts the app.
test_derived_data := "build/DerivedData-test"
# The builds with the scripted runs (`LEAL_BENCH`) for `bench-scroll` and
# `snapshot`. They have different build settings, so they never share
# products with `app`: the shipped app has no scripted runs.
bench_derived_data := "build/DerivedData-bench"

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
    cargo test --workspace --doc --exclude leal-ffi
    just doc

# Build the API docs, failing on any rustdoc warning (such as a broken intra-doc link).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --quiet

# `check`, then the app's XCTest suite. Must pass before a task's final commit.
check-all: check app-test

# Run all tests. Nextest does not run doctests, so they run separately.
test:
    cargo nextest run --workspace --all-features
    cargo test --workspace --doc --exclude leal-ffi

# Run all tests with many more property-test cases (the default is 256 per test).
test-deep cases="20000":
    PROPTEST_CASES={{cases}} cargo nextest run --workspace --all-features

# Format all code in place.
fmt:
    cargo fmt --all

# Run clippy on all crates and targets, with warnings as errors.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run the benchmarks (crates/leal-bench), passing any arguments to criterion: `just bench baseline/`.
bench *args:
    cargo bench --package leal-bench -- {{ args }}

# Generate the reference file (DESIGN §1) into target/bench-data/ and check its SHA-256.
reference-file:
    #!/usr/bin/env bash
    set -euo pipefail
    # Always regenerate (it takes well under a second in release), so this
    # checks the generator as it is now, not a file an older one cached.
    cargo run --release --quiet --package leal-bench --bin leal-refgen
    read -r expected name < crates/leal-bench/reference.sha256
    file="${LEAL_BENCH_DATA:-target/bench-data}/$name"
    actual="$(shasum -a 256 "$file" | cut -d ' ' -f 1)"
    if [ "$actual" != "$expected" ]; then
        echo "error: $file has SHA-256 $actual, expected $expected (crates/leal-bench/reference.sha256)" >&2
        exit 1
    fi
    echo "reference-file: $file matches crates/leal-bench/reference.sha256"

# Benchmark `base` and this checkout on this machine, one after the other, and report budgets, regressions and noise. CI runs it on each push to main.
bench-compare base="main" regression="0.20" noise="0.10":
    #!/usr/bin/env bash
    set -euo pipefail
    # Annotations on GitHub Actions, plain messages elsewhere.
    warn() { if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::warning::$*"; else echo "warning: $*" >&2; fi; }
    fail() { if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$*"; else echo "error: $*" >&2; fi; exit 1; }

    # Both sides share one criterion directory: `base` saves its results as
    # the `base` baseline, and this checkout's run compares with it. Running
    # both on the same machine within minutes is what makes the comparison
    # meaningful on a shared CI runner (docs/tasks/1.2b.md).
    root="$PWD"
    work="$root/target/bench-compare"
    export CRITERION_HOME="$work/criterion"
    # One reference file for both sides. Its name carries the generator's
    # version, so a base with a different generator writes its own file.
    export LEAL_BENCH_DATA="$root/target/bench-data"
    sha="$(git rev-parse --verify --quiet "{{ base }}^{commit}")" \
        || fail "bench-compare: \`{{ base }}\` is not a commit"

    # The base commit's files, without touching this checkout or git's
    # worktree list. `-m` gives the files the current time, so Cargo
    # rebuilds whatever changed since the last base it built. The base has
    # its own target directory: sharing one with this checkout could let
    # Cargo reuse one side's build for the other.
    rm -rf "$work/base"
    mkdir -p "$work/base"
    git archive "$sha" | tar -x -m -C "$work/base"
    has_base=yes
    if [ ! -d "$work/base/crates/leal-bench/benches" ]; then
        has_base=no
        warn "bench-compare: ${sha:0:12} ({{ base }}) has no benchmarks, so nothing is compared; only budgets are checked"
    fi

    # One side's benchmarks, in the directory $1 with the target directory
    # $2, passing the rest to criterion: everything (the `baseline` group
    # included), then that group again as `baseline-late`, to catch noise
    # that started part-way through. The steps are chained with `&&`
    # because `set -e` is off inside a function called with `||`.
    run_side() {
        local dir="$1" target="$2"
        shift 2
        (
            cd "$dir" \
            && CARGO_TARGET_DIR="$target" cargo bench --package leal-bench -- "$@" \
            && CARGO_TARGET_DIR="$target" LEAL_BENCH_BASELINE_GROUP=baseline-late \
                cargo bench --package leal-bench --bench baseline -- "$@"
        )
    }

    # A noisy run (a `memchr3_scan` canary moved) is rerun once. If the
    # rerun is noisy too, bench-report (given --last-attempt) warns that the
    # run is inconclusive and passes, unless a budget failed: a noisy runner
    # is no reason to turn CI red (docs/tasks/1.2b.md).
    for attempt in 1 2; do
        # Unquoted where it's used, so that empty means no argument.
        last_attempt=""
        if [ "$attempt" = 2 ] || [ "$has_base" = no ]; then
            last_attempt="--last-attempt"
        fi
        rm -rf "$CRITERION_HOME"
        mkdir -p "$CRITERION_HOME"
        if [ "$has_base" = yes ]; then
            run_side "$work/base" "$work/base-target" --save-baseline base \
                || fail "bench-compare: the benchmarks at ${sha:0:12} ({{ base }}) failed, so there is nothing to compare with"
            # Keep only the saved baseline, so a benchmark that this checkout
            # removed doesn't show up in the report with the base's numbers.
            find "$CRITERION_HOME" -type d -name new -prune -exec rm -rf {} +
            run_side "$root" "${CARGO_TARGET_DIR:-$root/target}" --baseline-lenient base
            echo "bench-compare: compared with ${sha:0:12} ({{ base }}), attempt $attempt"
        else
            run_side "$root" "${CARGO_TARGET_DIR:-$root/target}"
        fi

        status=0
        cargo run --release --quiet --package leal-bench --bin bench-report -- \
            --regression "{{ regression }}" --noise "{{ noise }}" $last_attempt \
            "$CRITERION_HOME" || status=$?
        case "$status" in
            0) exit 0 ;;
            3) warn "bench-compare: a canary moved by more than {{ noise }}, so the run was too noisy to judge; rerunning both sides once" ;;
            *) exit "$status" ;;
        esac
    done
    # Only reachable if bench-report asked for a rerun on the last attempt.
    fail "bench-compare: bench-report asked for a rerun after the last attempt"

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
    # both). Its library includes the test-only exports, so it builds into
    # its own DerivedData; the next `just ffi release` (from `just app
    # release` or an IDE build) rebuilds the library without them.
    configuration="$(just _configuration {{ profile }})"
    mkdir -p build
    log="build/xcodebuild-test.log"
    results="build/LealTests.xcresult"
    rm -rf "$results"
    status=0
    # LEAL_FFI_PREBUILT=1: see `app`. ENABLE_TESTABILITY=YES: the hosted
    # LealAppTests do `@testable import Leal`, which Release builds don't
    # allow by default. It applies only to this build, in its own
    # DerivedData, never to the app `just app release` makes.
    LEAL_FFI_PREBUILT=1 xcodebuild -quiet -project app/Leal.xcodeproj -scheme Leal \
        -configuration "$configuration" -derivedDataPath {{ test_derived_data }} \
        -destination "platform=macOS,arch=$(uname -m)" \
        -resultBundlePath "$results" \
        ENABLE_TESTABILITY=YES \
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

# Sync the String Catalog with the strings the app's code uses (DESIGN §4.4), from a Debug build.
strings: app
    xcrun xcstringstool sync app/Resources/Localizable.xcstrings \
        --stringsdata build/DerivedData/Build/Intermediates.noindex/Leal.build/Debug/Leal.build/Objects-normal/*/*.stringsdata

# Write a wide file for the scroll benchmark: 200 columns × 100,000 rows (about 130 MB) in target/bench-data/.
wide-file:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p target/bench-data
    awk 'BEGIN {
        srand(200);
        for (c = 1; c <= 200; c++) printf "%scol_%d", (c > 1 ? "," : ""), c; print "";
        for (r = 1; r <= 100000; r++) {
            for (c = 1; c <= 200; c++) {
                if (c % 3 == 0) v = sprintf("%.2f", rand() * 1000);
                else if (c % 3 == 1) v = "value " int(rand() * 100000);
                else v = int(rand() * 1000);
                printf "%s%s", (c > 1 ? "," : ""), v;
            }
            print "";
        }
    }' > target/bench-data/wide-200.csv
    ls -l target/bench-data/wide-200.csv

# Open `file` in a new Leal that scrolls itself frame by frame and print the frame times and memory (docs/tasks/1.6.md). Speed: fast or moderate. The app quits when it's done.
bench-scroll file speed="fast" profile="release" *options: (_app-scripted profile)
    #!/usr/bin/env bash
    set -euo pipefail
    # The sandboxed app writes into its container. `open` hands it the file,
    # so the sandbox lets it read it, and the options become argument
    # defaults (docs/tasks/1.6.md).
    container="$HOME/Library/Containers/io.github.robhaswell.leal/Data/tmp"
    out="bench-scroll-$$.json"
    app="$PWD/{{ bench_derived_data }}/Build/Products/$(just _configuration {{ profile }})/Leal.app"
    # A locked Mac turns its display off, and then the display link stops:
    # wake it and keep it on (the spike did the same, docs/tasks/0.4.md).
    caffeinate -u -t 2
    # In front: a display link doesn't fire for a window other windows cover.
    caffeinate -d just _run-scripted "$app" "{{ file }}" 300 front -LealBenchScroll "$out" -LealBenchSpeed "{{ speed }}" {{ options }}
    cat "$container/$out"
    rm -f "$container/$out"

# Draw `file`'s window offscreen to `out` (a PNG at 1×), with any of -LealAppearance dark, -LealSelect row,column, -LealJumpEnd YES, -LealSnapshotEarly YES. The app quits when it's done.
snapshot file out *options: (_app-scripted "debug")
    #!/usr/bin/env bash
    set -euo pipefail
    container="$HOME/Library/Containers/io.github.robhaswell.leal/Data/tmp"
    name="snapshot-$$.png"
    just _run-scripted "$PWD/{{ bench_derived_data }}/Build/Products/Debug/Leal.app" "{{ file }}" 60 back -LealSnapshot "$name" {{ options }}
    mv "$container/$name" "{{ out }}"
    echo "snapshot: {{ out }}"

# Build Leal.app with the scripted runs (the `LEAL_BENCH` compilation condition) into its own DerivedData.
[private]
_app-scripted profile: (ffi profile) xcodeproj
    #!/usr/bin/env bash
    set -euo pipefail
    configuration="$(just _configuration {{ profile }})"
    mkdir -p build
    log="build/xcodebuild-bench.log"
    # LEAL_FFI_PREBUILT=1: see `app`.
    LEAL_FFI_PREBUILT=1 xcodebuild -quiet -project app/Leal.xcodeproj -scheme Leal \
        -configuration "$configuration" -derivedDataPath {{ bench_derived_data }} \
        -destination "platform=macOS,arch=$(uname -m)" \
        'SWIFT_ACTIVE_COMPILATION_CONDITIONS=$(inherited) LEAL_BENCH' \
        build 2>&1 | tee "$log"
    just _no_warnings "$log"

# Fail if a Release app has the scripted runs (`LEAL_BENCH`): they write files and quit when given launch arguments. CI runs it after `just app release`.
check-no-bench app="build/DerivedData/Build/Products/Release/Leal.app":
    #!/usr/bin/env bash
    set -euo pipefail
    binary="{{ app }}/Contents/MacOS/Leal"
    if nm "$binary" | grep -E 'ScriptedRun|ScrollBench' || strings "$binary" | grep -E 'LealBenchScroll|LealSnapshot'; then
        echo "error: $binary has the scripted runs (LEAL_BENCH)" >&2
        exit 1
    fi
    echo "check-no-bench: $binary has no scripted runs"

# Open a file in a new Leal with scripted-run options (`place`: front, or back to leave the frontmost app alone), wait for it to quit by itself, and quit it after `limit` seconds.
[private]
_run-scripted app file limit place *options:
    #!/usr/bin/env bash
    set -uo pipefail
    background=""
    if [ "{{ place }}" = back ]; then background="-g"; fi
    open -n $background -W -a "{{ app }}" "{{ file }}" --args -ApplePersistenceIgnoreState YES {{ options }} &
    waiting=$!
    for _ in $(seq {{ limit }}); do
        sleep 1
        kill -0 "$waiting" 2>/dev/null || exit 0
    done
    echo "error: Leal didn't finish in {{ limit }} s; quitting it" >&2
    # The newest Leal: the one this started.
    pkill -n -x Leal
    exit 1

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

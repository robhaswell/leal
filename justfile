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
#
# `check` also lints the code as it ships, without those features: an item
# only test hooks use (a field only they read, say) warns only there. Only
# the libraries and binaries, not `--all-targets`: that builds leal-bench's
# benchmarks and tests, whose dev-dependency on leal-core turns on its
# `test-hooks` for the whole build. Both profiles, as release builds drop
# `debug_assertions` code.

# Format check, clippy (-D warnings; with and without test features, debug and release), tests, doctests and rustdoc (-D warnings). Must pass before every commit.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo clippy --workspace -- -D warnings
    cargo clippy --workspace --release -- -D warnings
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

# Run all tests with many more property-test cases (the default is 256 per test), passing any other arguments to nextest: `just test-deep 100000 --partition slice:1/6`.
test-deep cases="20000" *args:
    PROPTEST_CASES={{cases}} cargo nextest run --workspace --all-features --profile deep {{ args }}

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

# Benchmark `base` and this checkout on this machine, one after the other (a rerun swaps the order, and a third attempt reruns only the groups it must), and report budgets, regressions and noise. CI runs it on each push to main.
bench-compare base="main" regression="0.20" noise="0.10":
    #!/usr/bin/env bash
    set -euo pipefail
    # Annotations on GitHub Actions, plain messages elsewhere.
    warn() { if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::warning::$*"; else echo "warning: $*" >&2; fi; }
    fail() { if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$*"; else echo "error: $*" >&2; fi; exit 1; }

    # Both sides share one criterion directory: the side that runs first
    # saves its results as a baseline, and criterion compares the other
    # side's with it (see `measure`). Running
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
    # included, and each group's canaries, `canary/…`), then that group
    # again as `baseline-late`, to catch noise that started part-way
    # through. The steps are chained with `&&` because `set -e` is off
    # inside a function called with `||`.
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

    # One side's benchmarks for a third attempt, as `run_side`, but only
    # what the plan from bench-report selects: the run-wide canary, then
    # the bench groups to recheck and their canaries (in the bench targets
    # `$plan_benches` names, or all of them if it names none, filtered by
    # `$plan_filter`), then the late canary. If this side lacks a bench
    # target the plan names (a group moved between targets, say), every
    # target runs on this side, still filtered, so nothing is missed.
    run_groups() {
        local dir="$1" target="$2" bench every=no
        shift 2
        local benches=()
        for bench in $plan_benches; do
            if [ -f "$dir/crates/leal-bench/benches/$bench.rs" ]; then
                benches+=(--bench "$bench")
            else
                every=yes
            fi
        done
        if [ "$every" = yes ]; then
            benches=()
        fi
        (
            cd "$dir" \
            && CARGO_TARGET_DIR="$target" cargo bench --package leal-bench --bench baseline -- \
                "$@" '^baseline/memchr3_scan$' \
            && CARGO_TARGET_DIR="$target" cargo bench --package leal-bench \
                ${benches[@]+"${benches[@]}"} -- "$@" "$plan_filter" \
            && CARGO_TARGET_DIR="$target" LEAL_BENCH_BASELINE_GROUP=baseline-late \
                cargo bench --package leal-bench --bench baseline -- \
                "$@" '^baseline-late/memchr3_scan$'
        )
    }
    head_target="${CARGO_TARGET_DIR:-$root/target}"
    # Remove every benchmark's `new/`, its latest results.
    clear_new() { find "$CRITERION_HOME" -type d -name new -prune -exec rm -rf {} +; }

    # Both sides, in the order $1. The side that runs first saves its
    # results as a baseline, and criterion compares the second with it,
    # writing the median's change to `change/`. That leaves this
    # checkout's (head's) results in `new/`, where bench-report reads
    # them, and the order in `bench-compare-order`.
    #
    # - base-first: base saves `base`, head compares with it. The change
    #   is head's, relative to base.
    # - head-first: head saves `head`, base compares with it, and head's
    #   saved results then become `new/`. The change is base's, relative
    #   to head, and bench-report inverts it, because the order file says
    #   head-first (`report::collect`). So criterion's console lines on
    #   this attempt show the change the other way round.
    #
    # - third: as base-first, but only the groups to recheck (`run_groups`).
    #
    # Either way, a benchmark that only one side has isn't compared
    # (`--baseline-lenient`), and one that this checkout removed has no
    # `new/`, so it doesn't show up with the base's numbers.
    measure() {
        local order="$1" base_fails
        base_fails="bench-compare: the benchmarks at ${sha:0:12} ({{ base }}) failed, so there is nothing to compare with"
        case "$order" in
            base-first)
                run_side "$work/base" "$work/base-target" --save-baseline base || fail "$base_fails"
                clear_new
                run_side "$root" "$head_target" --baseline-lenient base
                ;;
            third)
                order=base-first
                run_groups "$work/base" "$work/base-target" --save-baseline base || fail "$base_fails"
                clear_new
                run_groups "$root" "$head_target" --baseline-lenient base
                ;;
            head-first)
                run_side "$root" "$head_target" --save-baseline head
                clear_new
                run_side "$work/base" "$work/base-target" --baseline-lenient head || fail "$base_fails"
                clear_new
                find "$CRITERION_HOME" -type f -path '*/head/benchmark.json' | while IFS= read -r file; do
                    mv "$(dirname "$file")" "$(dirname "$(dirname "$file")")/new"
                done
                ;;
        esac
        echo "$order" > "$CRITERION_HOME/bench-compare-order"
        echo "bench-compare: compared with ${sha:0:12} ({{ base }}), attempt $attempt, $order"
    }

    # Up to three attempts. A budget fails on any of them. A regression
    # fails only if every attempt shows it, and inconclusive (something
    # noise kept from being judged) warns and passes: a noisy runner is no
    # reason to turn CI red. A canary that moved by more than {{ noise }}
    # makes the run noisy if it is a run-wide one (`baseline/memchr3_scan`
    # or `baseline-late/memchr3_scan`), and the attempt noisy for a group's
    # benchmarks if it is one of that group's canaries. The rules are in
    # crates/leal-bench/src/report.rs, the history in docs/tasks/1.2b.md.
    #
    # 1. Both sides, base first. A noisy run or a regression is rerun.
    # 2. The rerun: both sides, head first, judged with attempt 1's results
    #    (--first-attempt). A regression on both attempts, both quiet for
    #    it, fails. If either was noisy for it, bench-report writes a plan
    #    (--plan) and asks for a third attempt.
    # 3. Only the groups in the plan, base first, judged with both earlier
    #    attempts (--second-attempt). A regression to recheck fails if
    #    attempt 3 shows it too, noisy for it or quiet, or has no result
    #    for it.
    #
    # Attempt 1 runs base first and the rerun head first. A drift over the
    # job (the runner warming up, a neighbour's load building) counts
    # against whichever side runs second: against head on attempt 1 and
    # against base on the rerun, so it can't fail both. With base first on
    # both attempts, it failed main at 2cc71bb (docs/tasks/1.2b.md).
    first="$work/criterion-attempt-1"
    second="$work/criterion-attempt-2"
    plan="$work/third-attempt-plan"
    plan_benches=""
    plan_filter=""
    rm -rf "$first" "$second" "$plan"
    for attempt in 1 2 3; do
        # An array, so that empty means no argument. The `+` form keeps an
        # empty one from tripping `set -u` in macOS's bash 3.2.
        report_args=()
        if [ "$has_base" = no ]; then
            report_args=(--last-attempt)
        elif [ "$attempt" = 2 ]; then
            report_args=(--first-attempt "$first" --plan "$plan")
        elif [ "$attempt" = 3 ]; then
            report_args=(--first-attempt "$first" --second-attempt "$second")
        fi
        rm -rf "$CRITERION_HOME"
        mkdir -p "$CRITERION_HOME"
        if [ "$has_base" = no ]; then
            run_side "$root" "$head_target"
        elif [ "$attempt" = 1 ]; then
            measure base-first
        elif [ "$attempt" = 2 ]; then
            measure head-first
        else
            measure third
        fi

        status=0
        cargo run --release --quiet --package leal-bench --bin bench-report -- \
            --regression "{{ regression }}" --noise "{{ noise }}" ${report_args[@]+"${report_args[@]}"} \
            "$CRITERION_HOME" || status=$?
        case "$status" in
            0) exit 0 ;;
            3)
                if [ "$attempt" = 1 ]; then
                    warn "bench-compare: a run-wide canary moved by more than {{ noise }} or a benchmark regressed; rerunning both sides once, head first this time (a regression fails only if every attempt shows it)"
                    mv "$CRITERION_HOME" "$first"
                elif [ "$attempt" = 2 ]; then
                    # The bench targets on the first line (none for all of
                    # them), the criterion filter on the second.
                    { read -r plan_benches; read -r plan_filter; } < "$plan"
                    warn "bench-compare: a benchmark regressed on both attempts, but at least one was noisy for it; rerunning only its groups, base first (bench targets: ${plan_benches:-all}; filter: $plan_filter). It fails only if this attempt shows the regression too"
                    mv "$CRITERION_HOME" "$second"
                else
                    break
                fi
                ;;
            *) exit "$status" ;;
        esac
    done
    # Only reachable if bench-report asked for another attempt after the third.
    fail "bench-compare: bench-report asked for another attempt after the third"

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

# Measure the app's DESIGN §1 budgets on this Mac and print the table (docs/perf.md): launch, open, index, scrolling (also while background work runs) and memory, 3 runs each, on the Release app launched with `open`. About 25 minutes; Leal's windows come to the front. Options go to leal-perf, e.g. `just perf --runs 1 --no-scroll`.
perf *options: (app "release") (_app-scripted "release") reference-file
    #!/usr/bin/env bash
    set -euo pipefail
    data="${LEAL_BENCH_DATA:-target/bench-data}"
    # The 1 GB variant for scrolling while a long index runs. Made once.
    big="$data/reference-v1-10m.csv"
    if [ ! -s "$big" ]; then
        cargo run --release --quiet --package leal-bench --bin leal-refgen -- --rows 10000000 --out "$big"
    fi
    cargo run --release --quiet --package leal-bench --bin leal-perf -- \
        --app "{{ derived_data }}/Build/Products/Release/Leal.app" \
        --bench-app "{{ bench_derived_data }}/Build/Products/Release/Leal.app" \
        --file "$data/reference-v1.csv" --big-file "$big" {{ options }}

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

# Fail if a Release app has leal-ffi's test-only exports (`test-exports`) or leal-core's test hooks (`test-hooks`): a test can make a document panic or pretend a drive vanished. CI runs it after `just app release`.
check-no-test-exports app="build/DerivedData/Build/Products/Release/Leal.app":
    #!/usr/bin/env bash
    set -euo pipefail
    # Plain `nm` (not `-gU`, which skips hidden symbols) sees the Rust
    # symbols, and `strings` UniFFI's metadata names, in either spelling
    # (`debug_panic`, `debugPanic`). Every test-only export starts `debug_`;
    # the test hooks are `open_simulating_*`, `SimulatedFault`,
    # `simulate_drive_back`, `simulate_clone_lost` (task 2.1a), and for
    # network shares (task 2.0)
    # `SimulatedShare`, `SimulatedShareFailure`, `simulated_share_reads`,
    # `simulated_share_release`, `simulated_head_reads`,
    # `share_reads_on_main_thread`, `debug_share_release`, and the private
    # `simulate_share_read`, `simulated_away`, `SimulatedState` and
    # `sleep_strictly`; and the copy held part-way, for the tests that pull
    # a real drive mid-copy: `open_holding_copy`,
    # `debug_open_document_holding_copy`, `release_held_copy`,
    # `debug_release_held_copy`, and the private `hold_copy_at`,
    # `wait_while_copy_held` and `HeldCopy`.
    pattern='debug_?(panic|watch|open_?document|simulate|share)|open_?simulating|simulated_?(fault|share|head|away|state)|simulate_?(drive|share|clone)|set_?fault|share_?reads_?on_?main|sleep_?strictly|hold(ing)?_?copy|held_?copy|copy_?held'
    for binary in "{{ app }}/Contents/Frameworks/LealFFI.framework/LealFFI" "{{ app }}/Contents/MacOS/Leal"; do
        if nm "$binary" | grep -Ei "$pattern" || strings "$binary" | grep -Ei "$pattern"; then
            echo "error: $binary has test-only exports or test hooks" >&2
            exit 1
        fi
    done
    echo "check-no-test-exports: no test-only exports or test hooks in {{ app }}"

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
    # Other agents run Leal test hosts and benchmarks on the same Mac, so
    # this never kills by name: only the Leal it started, by its PID. That
    # is the one process of this app bundle that wasn't running before.
    executable="{{ app }}/Contents/MacOS/Leal"
    before=" $(pgrep -f "$executable" | tr '\n' ' ') "
    open -n $background -W -a "{{ app }}" "{{ file }}" --args -ApplePersistenceIgnoreState YES {{ options }} &
    waiting=$!
    pid=""
    for _ in $(seq {{ limit }}); do
        sleep 1
        if [ -z "$pid" ]; then
            for candidate in $(pgrep -f "$executable"); do
                case "$before" in *" $candidate "*) ;; *) pid=$candidate ;; esac
            done
        fi
        kill -0 "$waiting" 2>/dev/null || exit 0
    done
    echo "error: Leal didn't finish in {{ limit }} s; quitting it" >&2
    if [ -n "$pid" ]; then kill "$pid"; fi
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

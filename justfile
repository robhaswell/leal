# Leal task runner. Run `just` to list the recipes.

# List the recipes.
default:
    @just --list

# Format check, clippy (-D warnings), tests and doctests. Must pass before every commit.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo nextest run --workspace
    cargo test --workspace --doc

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

# Build and launch Leal.app.
run:
    @echo "available from task 0.3"

# Build Leal.app.
app:
    @echo "available from task 0.3"

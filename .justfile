set shell := ["nu", "-l", "-c"]

# List the supported M0 workflows.
list:
    just --list

# Build the complete two-crate Rust workspace.
build:
    cargo build --release --workspace --locked

# Run the Windows capture server, forwarding its startup arguments.
capture *args:
    cargo run --release --locked -p capture-windows -- {{args}}

# Run the localhost canvas viewer on the requested port.
viewer port="4173":
    $env.LIVE_VITE_PORT = "{{port}}"; cd frontend; bun run dev

# Run the Rust workspace tests.
test:
    cargo test --release --workspace --all-features --locked

# Reject all Rust workspace lint warnings.
clippy:
    cargo clippy --release --workspace --all-targets --all-features --locked -- -D warnings

# Run every frontend validation before serving the viewer.
frontend-check:
    cd frontend; bun test
    cd frontend; bun run check
    cd frontend; bun run lint
    cd frontend; bun run build

# Run an arbitrary Bun command inside the frontend project.
bun *args:
    cd frontend; bun {{args}}

# Move a bookmark to a revision and publish it.
push bookmark="dev" revision="@-":
    jj bookmark move {{bookmark}} --to={{revision}}
    jj git push --all

# Fetch a bookmark and start a new working copy from its remote revision.
pull bookmark="dev":
    jj git fetch
    jj new -r {{bookmark}}@origin

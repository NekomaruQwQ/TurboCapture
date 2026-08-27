set shell := ["nu", "-l", "-c"]

alias c := check
alias b := build
alias i := build

# List the supported M0 workflows.
list:
    just --list

# Check the Rust backend and the frontend renderer.
check:
    cargo clippy -r
    cd frontend; bun run check
    cd frontend; bun run lint
    cd frontend; bun test

# Build the Rust backend and the frontend renderer.
build: compile-shaders
    cargo b -r
    cd frontend; bun run build

# Serve the frontend renderer on the configured port via http-server.
serve:
    cd frontend; bun run build
    cd frontend; http-server --port $env.TURBOCAPTURE_PORT

# Serve the frontend renderer on the configured port via vite.
dev:
    cd frontend; vite --port $env.TURBOCAPTURE_PORT

# Start the Windows capture server, forwarding its startup arguments.
capture *args: compile-shaders
    cargo run -r -p capture-windows -- {{args}}

# Compile the fixed-frame shaders consumed by capture-windows.
compile-shaders:
    cmd.exe /d /c crates\capture-windows\compile_fixed_frame_shaders.bat

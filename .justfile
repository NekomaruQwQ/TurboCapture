set shell := ["nu", "-l", "-c"]

# List the supported M0 workflows.
list:
    just --list

# Compile the fixed-frame shaders consumed by capture-windows.
shaders:
    cmd.exe /d /c crates\capture-windows\compile_fixed_frame_shaders.bat

# Build the Rust backend and the frontend renderer.
build: shaders
    cargo b -r
    cd frontend; bun run build

# Run the localhost canvas renderer on the requested port.
serve port:
    $env.LIVE_VITE_PORT = "{{port}}"; \
    cd frontend; bun run serve

# Run the Windows capture server, forwarding its startup arguments.
capture *args: shaders
    cargo run -r -p capture-windows -- {{args}}

# Run every frontend validation before serving the viewer.
check-frontend:
    cd frontend; bun test
    cd frontend; bun run check
    cd frontend; bun run lint
    cd frontend; bun run build

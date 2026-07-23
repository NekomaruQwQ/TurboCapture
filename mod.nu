# Nekomaru LiveUI — Nushell orchestration module.

# This module provides high-level commands to launch the various components
# of the livestreaming pipeline.

# ── Constants ──

# Directory where the compiled binaries are located. We use release binaries
# for better performance and consistency.

# ── Design Rules ──
#
# Every binary invocation goes through `get-exe`, which runs `cargo build
# --release --bin <name>` to ensure the binary is up-to-date.  Binaries that
# may run concurrently across launchers (live-capture, live-encoder, live-ws,
# live-app) use
# `get-exe --copy <id>` to copy the exe before spawning — this prevents file
# locking from blocking subsequent builds on Windows.

# ── Constants ──

const DEFAULT_AUDIO_DEVICE = "Loopback L + R (Focusrite USB Audio)"
const YOUTUBE_MUSIC_TITLE = "YouTube Music - Nekomaru LiveUI"
const REPO_ROOT = (path self | path dirname)

# ── Utility Commands ──

# Translate the repository's readable shader-stage names into Shader Model
# 5.0 target profiles understood by fxc. Keeping this mapping here makes an
# unsupported manifest entry fail before fxc receives an opaque target.
def shader-profile [stage: string]: nothing -> string {
    match $stage {
        "vertex"   => "vs_5_0"
        "pixel"    => "ps_5_0"
        "geometry" => "gs_5_0"
        "hull"     => "hs_5_0"
        "domain"   => "ds_5_0"
        "compute"  => "cs_5_0"
        _ => {
            error make -u {
                msg: $"Unsupported shader stage '($stage)' in shaders.toml."
            }
        }
    }
}

# Compile every shader entry declared in the repository-root shaders.toml.
# Output names include the entry point because one HLSL source may contain
# multiple stages, as resample.hlsl does.
export def compile-shaders []: nothing -> nothing {
    let manifest_path = $REPO_ROOT | path join "shaders.toml"
    let manifest = open $manifest_path

    for shader in $manifest.compile {
        let source_path = $REPO_ROOT | path join $shader.path
        let source = $source_path | path parse
        let output_path = $source.parent | path join $"($source.stem)_($shader.entry).fxo"
        let profile = shader-profile $shader.stage
        # Dependency paths are explicit and source-relative so Nushell does
        # not need to parse HLSL include directives or expand globs.
        let dependency_paths = (
            $shader
            | get -o dependencies
            | default []
            | each { |dependency| $source.parent | path join $dependency })
        let input_paths = $dependency_paths | prepend $source_path
        # Validate every declared input even when the output is missing. This
        # reports manifest typos directly instead of deferring them to fxc.
        let input_modified_times = $input_paths | each { |input_path|
            if not ($input_path | path exists) {
                error make -u { msg: $"Shader input not found: ($input_path)" }
            }
            ls $input_path | get modified.0
        }
        let needs_compile = if not ($output_path | path exists) {
            true
        } else {
            let output_modified = ls $output_path | get modified.0
            $input_modified_times | any { |input_modified|
                $input_modified > $output_modified
            }
        }

        if $needs_compile {
            print $"Compiling ($shader.path):($shader.entry) -> ($output_path)"
            ^fxc /nologo /O3 /Zi /WX /T $profile /E $shader.entry /Fo $output_path $source_path
        } else {
            print $"Up to date: ($shader.path):($shader.entry)"
        }
    }
}

# Build the specified Rust binary, optionally copy it with a new name, and
# return the path to the executable. This is useful for running multiple
# instances of the same binary simultaneously without blocking new builds.
export def --wrapped get-exe [name: string, --copy: string, ...args]: nothing -> string {
    const BINARY_DIR = (
        path self
            | path dirname
            | path join "target" "release")

    cargo build --release --bin $name

    let main_path = $"($BINARY_DIR)/($name).exe"
    if $copy == null {
        $main_path
    } else {
        let copy_path = $"($BINARY_DIR)/($name).($copy).exe"
        cp -f $main_path $copy_path
        $copy_path
    }
}

export def --env get-url [path: string = "/", --ws]: nothing -> string {
    get-url-precheck
    if ($env | get -o "LIVE_HOST") == null {
        check-env "LIVE_PORT"
        if not (patch-env "LIVE_HOST" $"localhost:($env.LIVE_PORT)") {
            error make -u { msg: "LIVE_HOST is required to run this command." }
        }
    }

    let protocol = if $ws { "ws" } else { "http" }
    $"($protocol)://($env.LIVE_HOST)($path)"
}

def --env get-url-precheck []: nothing -> nothing {
    if ($env | get -o "LIVE_HOST") != null {
        return
    }

    if ($env | get -o "LIVE_PORT") == null {
        error make -u { msg: "LIVE_HOST is required to run this command." }
    }

    print $"LIVE_HOST is required to run this command. Temporarily set LIVE_HOST according to LIVE_PORT? [Y/n]"
    let $input = try { input } catch {
        error make -u { msg: "Interrupted." }
    }

    if not ($input in ["Y", "y", ""]) {
        error make -u { msg: "Aborted." }
    }

    load-env { LIVE_HOST: $"localhost:($env.LIVE_PORT)" }
}

# ── Launcher for live-server ──

# Start the Rust/Axum server with Vite dev server proxied.
# Requires LIVE_PORT and LIVE_VITE_PORT environment variables.
export def --wrapped run-server [...args]: nothing -> nothing {
    if ($env | get -o "LIVE_PORT") == null {
        error make -u { msg: "LIVE_PORT is required to run this command." }
    }

    if ($env | get -o "LIVE_VITE_PORT") == null {
        error make -u { msg: "LIVE_VITE_PORT is required to run this command." }
    }

    ^(get-exe "live-server") ...$args
}

# ── Launcher for live-app ──

export def --wrapped run-app [...args]: nothing -> nothing {
    # Ensure LIVE_HOST is set for URL parsing.
    get-url | ignore

    (^(get-exe "live-app" --copy "app") (get-url)
        -x 1280 -y 720
        -t "Nekomaru LiveUI"
        ...$args)
}

export def --wrapped run-youtube-music [...args]: nothing -> nothing {
    # Ensure LIVE_HOST is set for URL parsing.
    get-url | ignore

    (^(get-exe "live-app" --copy "youtube-music")
        "https://music.youtube.com/"
        -x 1280 -y 720
        -t $YOUTUBE_MUSIC_TITLE
        ...$args)
}

# ── Capture and telemetry launchers ──

# Preview the local capture policy without encoding or network transport.
export def --wrapped "run-capture preview" [--config: path, ...args]: nothing -> nothing {
    # Keep the ignored personal example as the convenient default while still
    # allowing an explicit profile file as the first positional argument.
    let config_path = $config | default ($REPO_ROOT | path join "data" "selector-new.toml")
    (^(get-exe "live-capture" --copy "preview")
        --config $config_path
        ...$args)
}

# Start the supervised main stream from a local selector profile source.
# live-stream owns the encoder-to-relay pipe, resource-generation recovery,
# Job Object containment, and metadata posting. Selector policy remains a local
# TOML carried with this invocation and interpreted only by live-capture.
export def --wrapped "run-capture main" [--config: path, ...args]: nothing -> nothing {
    get-url | ignore
    let config_path = $config | default ($REPO_ROOT | path join "data" "selector-new.toml")
    let capture_path = get-exe "live-capture" --copy "main"
    let encoder_path = get-exe "live-encoder" --copy "main"
    let relay_path = get-exe "live-ws" --copy "main"
    let supervisor_path = get-exe "live-stream" --copy "main"

    (^$supervisor_path
        --mode main
        --capture $capture_path
        --encoder $encoder_path
        --relay $relay_path
        --config $config_path
        --server (get-url --ws "/internal/streams/main")
        --info-url (get-url "/internal/streams/main/info")
        ...$args)
}

# Start the supervised YouTube Music crop stream. The supervisor owns window
# discovery, DPI-aware crop policy, the generic crop encoder, and relay restarts.
export def --wrapped "run-capture youtube-music" [...args]: nothing -> nothing {
    get-url | ignore
    let capture_path = get-exe "live-capture" --copy "youtube-music"
    let encoder_path = get-exe "live-encoder" --copy "youtube-music"
    let relay_path = get-exe "live-ws" --copy "youtube-music"
    let supervisor_path = get-exe "live-stream" --copy "youtube-music"

    (^$supervisor_path
        --mode youtube-music
        --capture $capture_path
        --encoder $encoder_path
        --relay $relay_path
        --youtube-music-title $YOUTUBE_MUSIC_TITLE
        --server (get-url --ws "/internal/streams/youtube-music")
        ...$args)
}

# Start the audio capture pipeline.
# Captures desktop audio from the named WASAPI device and relays encoded
# PCM chunks via WebSocket.
export def run-audio [device: string = $DEFAULT_AUDIO_DEVICE]: nothing -> nothing {
    # Ensure LIVE_HOST is set for URL parsing.
    get-url | ignore

    (^(get-exe "live-audio")
        --device $device
    |^(get-exe "live-ws" --copy "audio")
        --mode audio
        --server (get-url --ws "/internal/audio"))
}

# Start the keystroke counter.
export def run-kpm []: nothing -> nothing {
    # Ensure LIVE_HOST is set for URL parsing.
    get-url | ignore

    (^(get-exe "live-kpm")
    |^(get-exe "live-ws" --copy "kpm")
        --server (get-url --ws "/internal/kpm"))
}

# ── Launcher for Claude Code usage poller ──

# Poll local Claude Code usage stats (via `ccusage`) and post today's
# totals to the server's string store every minute.  The server's
# `/internal/strings/:key` endpoint accepts arbitrary $-prefixed keys,
# so no server-side changes are needed.
#
# Today's date is recomputed each iteration so the loop self-corrects
# across midnight.  On `ccusage` failure (offline, not installed, no
# sessions yet) we log to stderr and skip the PUT — previous values
# stay visible until the next successful poll.
export def run-ccusage [--loop]: nothing -> nothing {
    # Ensure LIVE_HOST is set for URL parsing.
    get-url | ignore

    if not $loop {
        let today = (date now | format date "%Y%m%d")
        let totals = try {
            (bunx --bun ccusage daily --json --since $today
                | from json
                | get totals)
        } catch { |err|
            print -e $"ccusage failed: ($err.msg)"
            null
        }

        if $totals != null {
            http put (get-url "/internal/strings/$claudeTokens") ($totals.totalTokens | into string)
            http put (get-url "/internal/strings/$claudeCost")   ($totals.totalCost   | into string)
        }
    } else {
        loop {
            run-ccusage
            sleep 1min
        }
    }
}

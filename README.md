# TurboCapture

TurboCapture is a personal, source-run Windows capture service for livestream composition. One
`capture-windows` process selects one window, keeps the frame path on the GPU through H.264
encoding, and exposes that stream on a private localhost API. A small Chromium viewer decodes the
opaque video with WebCodecs and produces transparency in WebGL.

M0 deliberately targets one known Windows machine and browser stack. It is not packaged, portable,
authenticated, TLS-enabled, or intended for an untrusted network.

## Start one stream

Requirements: current Rust, Windows 11 and its SDK (`fxc.exe`), Bun, Nushell, `just`, a Chromium
browser with WebCodecs/WebGL2, and the exact adapter/Media Foundation encoder names for the capture
machine.

Create an ignored local configuration such as `data/minecraft.toml`:

```toml
[selection]
prefer_foreground = true

[[selection.profiles]]
name = "minecraft"
include = ["javaw.exe"]
exclude = []

[video]
width = 1920
height = 1080
frame_rate = 60
bit_rate = 12000000

[render.default]
key_colors = [[0, 255, 0]]
color_key_knee = { low = 0.02, high = 0.98 }
```

Then start the capture endpoint and viewer in separate terminals:

```console
just capture --config data/minecraft.toml --listen-address 127.0.0.1 --port 48100 --adapter-luid 0x000000000000F477 --encoder-name "NVIDIA H.264 Encoder MFT"
just viewer 4173
```

Open `http://127.0.0.1:4173/#/canvas?port=48100`. The same URL can be used as an iframe or browser
source; the `port` parameter belongs to the frontend route and selects
`ws://127.0.0.1:48100/api/video`.

For capture on another machine, forward its loopback endpoint onto the viewing machine first:

```console
ssh -N -L 48100:127.0.0.1:48100 capture-host
```

The browser still uses the localhost URL above, so M0 needs neither HTTPS nor a non-loopback viewer
origin.

## Development

```console
just build
just test
just clippy
just frontend-check
```

See [the M0 operating and architecture guide](docs/README.md),
[the governing principles](docs/M0-Principles.md), and
[the migration plan](docs/M0-Migration-Plan.md) for the complete contract.

## FILE-OWNERSHIP: Per-file ownership tracking

**agent** = Claude manages.
**human** = Nekomaru hand-crafts.

```bash
# == Configurations & Documentations

## Config files
.gitignore                                  human
.justfile                                   human
mod.nu                                      human
biome.json                                  human
Cargo.toml                                  human
FILE-OWNERSHIP.md                           human
frontend/package.json                       human
frontend/tsconfig.json                      human
frontend/vite.config.ts                     human

## Config.toml files
live-app/Cargo.toml                         human
live-audio/Cargo.toml                       human
live-protocol/Cargo.toml                    human
live-selector/Cargo.toml                    human
live-ws/Cargo.toml                          human
live-encoder/Cargo.toml                     human
live-capture-youtube-music/Cargo.toml       human
live-kpm/Cargo.toml                         human
live-server/Cargo.toml                      human
crates/enumerate-windows/Cargo.toml         human
crates/set-dpi-awareness/Cargo.toml         human

## docs/
ARCHIVE-0-Prototype.md                      agent
ARCHIVE-1-StreamID.md                       agent
M4-DESIGN.md                                agent
PLAN-UI-AudioMeter.md                       agent
PLAN-UI-KPMMeter.md                         agent
README.md                                   agent
README-Audio.md                             agent

# == Rust crates ==

## live-app/
src/main.rs                                 human

## live-audio/
src/main.rs                                 agent

## live-protocol/
src/lib.rs                                  agent
src/audio.rs                                agent
src/avcc.rs                                 agent
src/video.rs                                agent

## live-ws/
src/main.rs                                 agent

## live-selector/
src/main.rs                                 agent
src/presenter.rs                            agent
src/capture.rs                              agent
src/d3d11.rs                                agent
src/resample.rs                             human
src/resample.hlsl                           human
src/selector/mod.rs                         agent
src/selector/config.rs                      agent

## live-capture-youtube-music/
src/main.rs                                 agent
src/crop.rs                                 agent

## live-encoder/
src/main.rs                                 agent
src/lib.rs                                  agent
src/capture.rs                              agent
src/converter.rs                            agent
src/d3d11.rs                                agent
src/encoder.rs                              agent
src/encoder/debug.rs                        agent
src/encoder/helper.rs                       agent
src/resample.hlsl                           human
src/resample.rs                             human
src/selector/mod.rs                         agent
src/selector/config.rs                      agent

## live-kpm/
src/main.rs                                 agent
src/hook.rs                                 agent
src/calculator.rs                           agent
src/message_pump.rs                         agent

## live-server/
src/main.rs                                 agent
src/state.rs                                agent
src/video.rs                                agent
src/audio.rs                                agent
src/kpm.rs                                  agent
src/strings.rs                              agent
src/selector.rs                             agent
src/events.rs                               agent
src/vite_proxy.rs                           agent

## crates/enumerate-windows/
src/lib.rs                                  human
src/main.rs                                 agent

## crates/set-dpi-awareness/
src/lib.rs                                  human

# == React frontend ==

## frontend/
debug.ts                                    human
global.css                                  human
global.effects.css                          human
global.tailwind.css                         human
index.html                                  human
index.tsx                                   human

## frontend/src/
api.ts                                      agent
streams.ts                                  agent
strings.ts                                  agent
strings-api.ts                              agent
ws.ts                                       agent
app.tsx                                     human
kpm.tsx                                     agent

## frontend/src/audio/
index.tsx                                   agent
worklet.ts                                  agent
worklet-env.d.ts                            agent

## frontend/src/components/
grid.tsx                                    human
marquee.tsx                                 agent

## frontend/src/widgets/
common.tsx                                  human
index.tsx                                   human

## frontend/src/video/
color-key.ts                                agent
decoder.ts                                  agent
index.tsx                                   agent
```

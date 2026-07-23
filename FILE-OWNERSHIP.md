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
frontend/svelte.config.ts                   human
frontend/vite.config.ts                     human
frontend/vite.d.ts                          human
shaders.toml                                human

## Cargo.toml files
live-app/Cargo.toml                         human
live-audio/Cargo.toml                       human
live-protocol/Cargo.toml                    human
live-capture/Cargo.toml                     human
live-ws/Cargo.toml                          human
live-encoder/Cargo.toml                     human
live-kpm/Cargo.toml                         human
live-server/Cargo.toml                      human
live-stream/Cargo.toml                      human
crates/live-shared-texture/Cargo.toml       human
crates/enumerate-windows/Cargo.toml         human
crates/set-dpi-awareness/Cargo.toml         human

## docs/
ARCHIVE-M0-Prototype.md                     agent
ARCHIVE-M4-DESIGN.md                        agent
ARCHIVE-M4-KPMMeter.md                      agent
ARCHIVE-M4-StreamSupervisor.md              agent
PLAN-UI-AudioMeter.md                       agent
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

## live-stream/
src/main.rs                                 agent
src/metadata.rs                             agent
src/restart.rs                              agent
src/youtube_music.rs                        agent

## live-capture/
src/main.rs                                 agent
src/presenter.rs                            agent
src/publisher.rs                            agent
src/capture.rs                              agent
src/d3d11.rs                                agent
src/resample.rs                             human
src/resample.hlsl                           human
src/selector/mod.rs                         agent
src/selector/config.rs                      agent

## crates/live-shared-texture/
src/lib.rs                                  agent

## live-encoder/
src/main.rs                                 agent
src/lib.rs                                  agent
src/converter.rs                            agent
src/d3d11.rs                                agent
src/encoder.rs                              agent
src/encoder/debug.rs                        agent
src/encoder/helper.rs                       agent
src/pipeline.rs                             agent

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
src/events.rs                               agent
src/events_ws.rs                            agent
src/util.rs                                 agent
src/vite_proxy.rs                           agent

## crates/enumerate-windows/
src/lib.rs                                  human
src/main.rs                                 agent

## crates/set-dpi-awareness/
src/lib.rs                                  human

# == Svelte frontend ==

## frontend/
debug.ts                                    human
global.css                                  human
global.effects.css                          human
global.tailwind.css                         human
index.html                                  human
index.ts                                    human

## frontend/src/
api.ts                                      agent
events.svelte.ts                            agent
streams.svelte.ts                           agent
ws.ts                                       agent
App.svelte                                  human
KpmMeter.svelte                             agent

## frontend/src/audio/
AudioStream.svelte                          agent
worklet.ts                                  agent
worklet-env.d.ts                            agent

## frontend/src/components/
Grid.svelte                                 human
Icon.svelte                                 human
Marquee.svelte                              agent

## frontend/src/widgets/
AboutWidget.svelte                          human
ClaudeUsageWidget.svelte                    human
ClockWidget.svelte                          human
LiveModeWidget.svelte                       human
LiveWidget.svelte                           human

## frontend/src/video/
color-key.ts                                agent
decoder.ts                                  agent
stream-loop.ts                              agent
StreamRenderer.svelte                       agent
```

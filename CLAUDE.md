# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build Commands

```bash
# First-time setup (pjproject is a git submodule)
git submodule update --init --recursive

# Build (first build compiles pjproject from source via build.rs - takes a while)
cargo build
cargo build --release

# Run
./target/release/mumble-sip config.toml

# Docker Compose (Docker assets are in docker/)
docker compose -f docker/docker-compose.yml up -d --build

# Multi-arch image builds
docker buildx build --platform linux/amd64,linux/arm64 -f docker/Dockerfile -t yourorg/mumble-sip:latest --push .
docker buildx build --platform linux/amd64,linux/arm64 -f docker/Dockerfile.pocket-tts -t yourorg/pocket-tts:latest --push .

# Check / lint
cargo check
cargo clippy

# Run tests (unit tests exist in tts.rs, control.rs, config.rs, db.rs)
cargo test
cargo test --lib -- tts::tests          # TTS tests only
cargo test --lib -- control::tests      # Mumble control tests only
cargo test -- db::tests                 # Database tests only
```

## System Dependencies

Building requires: gcc, make, binutils, protoc, libssl-dev, libasound2-dev, libopus-dev, uuid-dev.

## Architecture

SIP-to-Mumble audio bridge. Receives inbound SIP calls and routes bidirectional audio into Mumble servers. Each call gets its own independent Mumble connection.

### Workspace Layout

- **Root crate (`mumble-sip`)** - Main application
- **`pjsip-sys/`** - FFI bindings subcrate. Builds pjproject v2.16 from a git submodule (`pjsip-sys/pjproject/`), generates Rust bindings via bindgen. The `build.rs` handles configure, make, library linking, and binding generation.

### Threading Model

Two worlds that must be bridged:

1. **PJSIP threads** - C library with its own thread pool. Callbacks (`put_frame`/`get_frame`) run on pjsip's media clock thread. Any tokio worker calling pjsua APIs must first register with `sip::ensure_pj_thread_registered()` (must be called after every `.await` since tokio may resume on a different worker).

2. **Tokio runtime** - All Mumble networking, Opus encoding/decoding, and session management.

**Bridge mechanism:** Lock-free SPSC ring buffers (`ringbuf` crate) carry PCM samples between pjsip's media thread and tokio tasks. An `mpsc` channel carries SIP events (incoming call, state change, media active, DTMF digit) from C callbacks to the async event loop.

### Per-Call Audio Pipeline

```
SIP caller <-> pjmedia_port (put_frame/get_frame)
                    |  ringbuf (lock-free SPSC)
              encoder task: PCM -> Opus -> mpsc -> MumbleSender -> Mumble server
              decoder task: Mumble -> Opus -> mpsc -> PCM -> ringbuf -> pjmedia_port
```

Each call spawns 6 tokio tasks: encoder, decoder, voice forwarder, mumble event handler, TTS announcement worker, and TTS text message worker.

The decoder (`spawn_mixed_decoder`) maintains per-speaker Opus decoder state and mixes all active speakers plus sound effects (join/leave chimes, TTS audio) into a single 10ms output frame per tick.

### Key Patterns

- **`SendablePort`** - Wrapper newtype to make raw C pointers `Send`+`Sync` for storage in `HashMap` behind `Mutex`.
- **`MumbleClient` / `MumbleSender`** - `MumbleClient` owns the connection; `MumbleSender` is a clonable handle (wraps `mpsc::UnboundedSender`) for sending voice from other tasks.
- **Pending ports** - Media ports are registered in a global `PENDING_PORTS` map by the session manager, then consumed by pjsip's `on_call_media_state` callback when media becomes active.
- **Conference port cleanup** - Uses `on_conf_op_completed` callback to detect when `pjsua_conf_remove_port` finishes asynchronously. Cleanup data is registered in `PENDING_CLEANUP` before removal, then the callback destroys the port and releases the pool. Falls back to deferred cleanup (250ms delay on a dedicated OS thread) if the remove fails.
- **Bridge session tracking** - `bridge_mumble_sessions` (`HashSet<u32>`) tracks Mumble session IDs belonging to this bridge instance. Audio from these sessions is suppressed to prevent feedback loops between co-located SIP callers on the same Mumble server.
- **Silence detection** - Encoder uses average absolute sample threshold with a 200ms hold period before sending an end-of-transmission Opus frame, matching mumble-web2 behavior.
- **`PJ_SUCCESS`** is a C macro (value `0`), not exported by bindgen. Compare status codes against `0` directly.

### Call Routing & DTMF Navigation

The custom `X-Mumble-Server` SIP header (extracted in `callbacks.rs`) routes calls to different Mumble servers. Without the header, the default from `config.toml` is used. The caller's phone number is extracted from the SIP From URI and used to look up a persistent nickname from the database.

DTMF digits `*`/`#` navigate to previous/next Mumble channel. `1` replays the intro message. Channel changes are communicated to the event handler via a `tokio::sync::watch` channel. On reconnect, callers automatically rejoin the last channel they were in (per Mumble server), looked up from the `caller_channels` table.

### Intro Auto-Play

First-time callers (`is_new` flag from `CallerStore`) or callers returning after a configurable absence (`audio.intro_replay_after_days`, default 30) automatically hear the intro message. The intro plays **before** the Mumble connection is established — the SIP call is answered and audio pipeline started, but Mumble connect is deferred until the intro finishes. This ensures the caller hears the intro without Mumble chatter and is not visible to Mumble users until ready. The `CallerInfo.last_seen` field returns the **previous** value (before updating to now) to enable staleness checks.

### TTS & Sound Effects

- **Sound effects** (`src/audio/sounds.rs`): WAV files in `sounds/` are embedded via `include_bytes!` and lazily decoded to 48kHz PCM on first use. Events: `SelfJoinedChannel`, `UserJoinedChannel`, `UserLeftChannel`, `TextMessage`, `Intro`. The `sound_duration()` helper computes playback duration from PCM sample count at runtime.
- **Pocket-TTS** (`src/audio/tts.rs`): Optional external HTTP service (typically a Docker Compose sibling). Synthesizes channel-name announcements and text message speech. Uses LRU cache (64 entries). Falls back to chime sound effects when TTS is unavailable. Announcement debouncing prevents rapid-fire channel-change speech.
- **Link-aware text messages**: HTML anchor tags are parsed for `href` URLs; bare hostnames are detected. URLs are converted to spoken form ("google dot com"). Link-only messages use "posted a link to" phrasing.

### Persistent Storage

- **SQLite database** (`src/db.rs`): Stores caller data using `sqlx` with async SQLite. The `CallerStore` trait abstracts the storage backend for future portability (e.g. Postgres).
- **Caller nicknames**: Phone numbers are never exposed as Mumble usernames. Each caller gets a Docker-style generated name (e.g. "relaxed_babbage") via the `names` crate, persisted in the `callers` table keyed by phone number.
- **Last channel persistence**: The `caller_channels` table maps `(phone_number, server_host)` → `channel_id`. On disconnect, the caller's current channel is saved; on reconnect, they rejoin it automatically. DB writes are spawned as tokio tasks and tracked in `pending_writes` (`Vec<JoinHandle<()>>`), reaped at each disconnect and drained with a 60s timeout at shutdown.
- **Schema migrations**: Managed via `sqlx::migrate!()`, migration files live in `migrations/`. The database is created automatically on first run.
- **Config**: `[database]` section in `config.toml` with `path` field (default: `mumble-sip.db`).

### Audio Constants

- Opus frames: 10ms / 480 samples at 48kHz mono (matching Mumble's `iFrameSize`)
- PJSIP media clock: 48kHz to match Opus/Mumble
- Ring buffer capacity: ~200ms per direction
- Encoder polls every 5ms for low latency
- Jitter buffer: configurable, default 60ms (6 frames), minimum 2 frames

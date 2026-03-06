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

# Check / lint
cargo check
cargo clippy
```

There are no tests in this project currently.

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

**Bridge mechanism:** Lock-free SPSC ring buffers (`ringbuf` crate) carry PCM samples between pjsip's media thread and tokio tasks. An `mpsc` channel carries SIP events (incoming call, state change, media active) from C callbacks to the async event loop.

### Per-Call Audio Pipeline

```
SIP caller <-> pjmedia_port (put_frame/get_frame)
                    |  ringbuf (lock-free SPSC)
              encoder task: PCM -> Opus -> mpsc -> MumbleSender -> Mumble server
              decoder task: Mumble -> Opus -> mpsc -> PCM -> ringbuf -> pjmedia_port
```

Each call spawns 4 tokio tasks: encoder, decoder, voice forwarder, mumble event handler.

### Key Patterns

- **`SendablePort` / `SendablePool`** - Wrapper newtypes to make raw C pointers `Send`+`Sync` for storage in `HashMap` behind `Mutex`.
- **`MumbleClient` / `MumbleSender`** - `MumbleClient` owns the connection; `MumbleSender` is a clonable handle (wraps `mpsc::UnboundedSender`) for sending voice from other tasks.
- **Pending ports** - Media ports are registered in a global `PENDING_PORTS` map by the session manager, then consumed by pjsip's `on_call_media_state` callback when media becomes active.
- **Deferred cleanup** - Conference bridge port teardown uses a 100ms delay on a dedicated OS thread to ensure pjsip's clock thread has finished its current tick before freeing memory.
- **`PJ_SUCCESS`** is a C macro (value `0`), not exported by bindgen. Compare status codes against `0` directly.

### Call Routing

The custom `X-Mumble-Server` SIP header (extracted in `callbacks.rs`) routes calls to different Mumble servers. Without the header, the default from `config.toml` is used. The caller's phone number (from SIP From URI) becomes the Mumble username.

### Audio Constants

- Opus frames: 10ms / 480 samples at 48kHz mono (matching Mumble's `iFrameSize`)
- PJSIP media clock: 48kHz to match Opus/Mumble
- Ring buffer capacity: ~200ms per direction
- Encoder polls every 5ms for low latency

# mumble-sip

A SIP-to-Mumble audio bridge. Receives inbound SIP calls and routes audio bidirectionally into Mumble servers. Each call gets its own independent Mumble connection, and calls can be routed to different Mumble servers using a custom `X-Mumble-Server` SIP header.

## Features

- Multiple simultaneous SIP calls, each with its own Mumble bot user
- Per-call Mumble server routing via `X-Mumble-Server` SIP header
- Opus audio encoding/decoding at 48kHz
- Lock-free audio pipeline between PJSIP and Mumble
- Configurable max concurrent calls
- DTMF Navigation
  - `*` for previous channel
  - `#` for next channel

## Dependencies

**System packages:**
- binutils, make, gcc (for building PJSIP)
- [protoc](https://protobuf.dev/installation/) (for Mumble protocol buffers)
- ALSA development libraries (`libasound2-dev` on Debian/Ubuntu)
- OpenSSL development libraries (`libssl-dev`)
- Opus development library (`libopus-dev`)
- UUID library (`uuid-dev`)
- Rust / cargo

## Building

```bash
git clone --recursive https://github.com/youruser/mumble-sip.git
cd mumble-sip

# If you already cloned without --recursive:
git submodule update --init --recursive

cargo build --release
```

## Configuration

Copy `config.toml.example` to `config.toml` and edit it:

```toml
[sip]
listen_port = 5060
account_uri = "sip:bridge@pbx.example.com"
registrar = "sip:pbx.example.com"
username = "bridge"
password = "secret"
max_concurrent_calls = 10

[mumble]
default_host = "mumble.example.com"
port = 64738
username = "SIP-Bridge"
password = ""
channel = "SIP Calls"
accept_invalid_cert = true

[audio]
sample_rate = 48000
frame_duration_ms = 20
opus_bitrate = 32000
jitter_buffer_ms = 60
```

## Usage

```bash
./target/release/mumble-sip config.toml
```

## Asterisk Integration

### PJSIP Endpoint

Define an endpoint in `pjsip.conf` (or the equivalent in your Asterisk config) for the mumble-sip bridge:

```ini
; pjsip.conf

[mumble-bridge-transport]
type = transport
protocol = udp
bind = 0.0.0.0:5060

[mumble-bridge]
type = endpoint
transport = mumble-bridge-transport
context = mumble-bridge
disallow = all
allow = ulaw
allow = alaw
aors = mumble-bridge

[mumble-bridge]
type = aor
contact = sip:bridge@10.0.0.50:5060  ; IP/port of mumble-sip server

[mumble-bridge]
type = identify
endpoint = mumble-bridge
match = 10.0.0.50  ; IP of mumble-sip server
```

### Dialplan — Basic

Route a specific extension to the bridge. All calls go to the default Mumble server from `config.toml`:

```ini
; extensions.conf

[default]
exten => 7000,1,NoOp(Routing to Mumble bridge)
 same => n,Dial(PJSIP/mumble-bridge)
 same => n,Hangup()
```

### Dialplan — With X-Mumble-Server Header

Route calls to different Mumble servers based on the dialed extension using the custom `X-Mumble-Server` header:

```ini
; extensions.conf

[default]
; Extension 7001 → mumble-server-a.example.com
exten => 7001,1,NoOp(Routing to Mumble Server A)
 same => n,Set(PJSIP_HEADER(add,X-Mumble-Server)=mumble-server-a.example.com)
 same => n,Dial(PJSIP/mumble-bridge)
 same => n,Hangup()

; Extension 7002 → mumble-server-b.example.com
exten => 7002,1,NoOp(Routing to Mumble Server B)
 same => n,Set(PJSIP_HEADER(add,X-Mumble-Server)=mumble-server-b.example.com)
 same => n,Dial(PJSIP/mumble-bridge)
 same => n,Hangup()

; Extension 7003 → uses default from config.toml (no header)
exten => 7003,1,NoOp(Routing to default Mumble server)
 same => n,Dial(PJSIP/mumble-bridge)
 same => n,Hangup()
```

### Dialplan — Dynamic Server from Database

For dynamic routing, you can look up the Mumble server from a database or variable:

```ini
; extensions.conf

[mumble-rooms]
exten => _70XX,1,NoOp(Dynamic Mumble routing for ${EXTEN})
 same => n,Set(MUMBLE_HOST=${DB(mumble/servers/${EXTEN})})
 same => n,GotoIf($["${MUMBLE_HOST}" = ""]?no_server)
 same => n,Set(PJSIP_HEADER(add,X-Mumble-Server)=${MUMBLE_HOST})
 same => n,Dial(PJSIP/mumble-bridge)
 same => n,Hangup()
 same => n(no_server),Playback(ss-noservice)
 same => n,Hangup()
```

Populate the AstDB entries:
```bash
asterisk -rx 'database put mumble/servers 7010 mumble-a.example.com'
asterisk -rx 'database put mumble/servers 7011 mumble-b.example.com'
```

### Legacy chan_sip

If using the older `chan_sip` driver instead of PJSIP:

```ini
; sip.conf

[mumble-bridge]
type = peer
host = 10.0.0.50
port = 5060
disallow = all
allow = ulaw
allow = alaw
context = mumble-bridge
```

```ini
; extensions.conf

[default]
exten => 7001,1,NoOp(Routing to Mumble via chan_sip)
 same => n,SIPAddHeader(X-Mumble-Server: mumble-server-a.example.com)
 same => n,Dial(SIP/mumble-bridge)
 same => n,Hangup()
```

## Architecture

```
PJSIP thread(s)                        Tokio runtime
+-----------------------+              +-----------------------------------+
| SIP call events       |---mpsc------>| SIP event handler                 |
| (all calls)           |              |  on IncomingCall: spawn session    |
+-----------------------+              |  on Disconnect: tear down          |
                                       |                                   |
Per-call:                              |                                   |
+-------------------+                  |                                   |
| Call N media port |--ringbuf-------->| Session N: PCM -> Opus -> Mumble  |
|  put_frame (PCM)  |                 | Session N: Mumble -> Opus -> PCM  |
|  get_frame (PCM)  |<--ringbuf------+|                                   |
+-------------------+                  +-----------------------------------+
```
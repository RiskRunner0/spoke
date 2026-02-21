# Spoke

A gaming-focused Matrix client with voice chat. Built in Rust using egui and LiveKit.

## Stack

- **UI**: egui/eframe
- **Matrix**: matrix-rust-sdk 0.8 + Conduit homeserver
- **Voice**: LiveKit Rust SDK + CPAL audio
- **Sidecar**: `spoke-sidecar` — validates Matrix tokens, issues LiveKit JWTs

## Prerequisites

```
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Docker (for Conduit + LiveKit)
# Install via your distro's package manager

# Native deps for livekit (builds libwebrtc from source — one-time, ~10-30 min)
# Debian/Ubuntu:
sudo apt install clang cmake libssl-dev

# Arch/CachyOS:
sudo pacman -S clang cmake openssl
```

## Local development

### 1. Start Conduit + LiveKit

```bash
docker compose -f infra/docker-compose.dev.yml up -d
```

This starts:
- **Conduit** (Matrix homeserver) on `http://localhost:8448` — open registration, no TLS
- **LiveKit** on `ws://localhost:7880` — dev mode with the key `devkey`

### 2. Start the sidecar

The sidecar validates Matrix access tokens and issues LiveKit JWTs.

```bash
cargo run -p spoke-sidecar
```

Default env vars (override as needed):

| Variable        | Default                              | Description                  |
|-----------------|--------------------------------------|------------------------------|
| `LIVEKIT_URL`   | `ws://localhost:7880`                | LiveKit server URL           |
| `LIVEKIT_KEY`   | `devkey`                             | LiveKit API key              |
| `LIVEKIT_SECRET`| `devsecretatmostthirtytwocharslong`  | LiveKit API secret           |
| `MATRIX_SERVER` | `http://localhost:8448`              | Matrix homeserver to validate against |
| `PORT`          | `8090`                               | Sidecar listen port          |
| `TURN_SECRET`   | *(unset)*                            | Optional TURN shared secret  |
| `TURN_HOST`     | *(unset)*                            | Optional TURN hostname       |

### 3. Run the app

```bash
SPOKE_HS=http://localhost:8448 \
SPOKE_USER=alice \
SPOKE_PASS=password \
SPOKE_SIDECAR=http://localhost:8090 \
cargo run -p spoke-app
```

Setting all three of `SPOKE_HS`, `SPOKE_USER`, `SPOKE_PASS` causes the app to log in automatically on launch. If any are unset, a login screen is shown instead.

### 4. Test voice

1. Open a second terminal and run the app again with different credentials (e.g. `SPOKE_USER=bob`). Both users must share a room.
2. In user A's window: select the shared room, click **Join Voice**.
3. In user B's window: select the same room, click **Join Voice**.
4. Both users should hear each other. The sidebar shows connected participants.
5. Click **Mute** to silence your microphone. Click **Leave Voice** to disconnect.

### Tear down

```bash
docker compose -f infra/docker-compose.dev.yml down -v   # -v wipes stored Matrix state
```

## Project layout

```
spoke/
├── infra/
│   ├── docker-compose.dev.yml   # Conduit + LiveKit
│   ├── conduit.dev.toml         # Conduit config
│   └── livekit.dev.yaml         # LiveKit dev config
├── spoke-core/                  # Async library (Matrix client + voice session)
│   └── src/voice/
│       ├── mod.rs               # VoiceSession — LiveKit room connect/disconnect
│       ├── audio.rs             # CPAL mic capture + speaker playback
│       └── events.rs            # org.spoke.voice.* Matrix event types
├── spoke-sidecar/               # Axum service: POST /_spoke/v1/voice/token
└── spoke-app/                   # egui desktop app
    └── src/
        ├── main.rs
        ├── app.rs               # UI (rooms, messages, voice controls)
        └── bridge.rs            # Async/sync bridge (Matrix task ↔ egui)
```

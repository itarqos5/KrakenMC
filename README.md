<div align="center">
  <h1>🦑 Kraken Minecraft Server</h1>
  <p><strong>High-performance Minecraft Java Edition proxy/server backend built in Rust with Bevy ECS.</strong></p>
</div>

---

## Overview

Kraken is a Rust-based Minecraft server engine designed for high-throughput Java Edition protocol handling. It acts as a protocol-aware bridge (via the ViaKraken plugin) and includes a modular ECS-driven backend built on Bevy.

## ✨ Features

- 🦀 **Core:** Rust Nightly with `portable_simd` support for maximum throughput.
- ⚙️ **Network Engine:** Native Minecraft Java protocol implementation via **[Azalea](https://github.com/mat-1/azalea)** primitives, supporting protocols **774 (1.21.11)** and **775 (26.1)**.
- 🧩 **Architecture:** Driven by **Bevy ECS** for modular systems and concurrent game loops.
- 💾 **Persistence:** Embedded **Sled** key-value store with **Postcard + Flate2 (Gzip)** compression for player states.
- 🌉 **ViaKraken Bridge:** Custom Bevy plugin intercepting handshakes and login packets for seamless protocol mapping.

## 🚀 Getting Started

### Prerequisites

Kraken requires the **Rust Nightly** toolchain due to `portable_simd` usage in dependencies:

```bash
rustup toolchain install nightly
rustup override set nightly
```

### First Launch — EULA Gate

Before any network listeners or game loops start, Kraken validates the EULA:

- If `eula.txt` does **not** exist, the server creates it with `eula=false`, generates a default `server.properties`, logs an alert, and **halts immediately**.
- If `eula.txt` exists but contains `eula=false`, the server logs an alert and halts.

The engine **will not bind to port 25565** or initialize internal systems until `eula=true` is set. Edit `eula.txt` to accept the Mojang EULA before restarting.

### Installation & Running

1. **Clone the repository:**
   ```bash
   git clone https://github.com/itarqos5/KrakenMC.git
   cd KrakenMC
   ```

2. **Build and run:**
   ```bash
   cargo run --release
   ```

## 🖥️ Platform Notes

### Windows ANSI Console Colors

On Windows, Kraken forces **native ANSI escape-code parsing** via a Kernel32 `SetConsoleMode(handle, ENABLE_VIRTUAL_TERMINAL_PROCESSING)` call. This prevents broken text tokens in legacy `cmd.exe` windows.

## 🏗️ Technical Architecture

### Protocol 26.1 (775) — LoginSuccess Trailing Boolean

Minecraft 26.1 appends a trailing 1-byte Boolean flag after the player UUID/Username payload in `ClientboundLoginFinishedPacket` (ID `0x02`). Kraken's decoder **explicitly consumes** this boolean under Protocol 775, emptying the buffer and eliminating the `found N bytes extra` DecoderException.

### Persistence Lifecycle

Kraken uses a **Dirty Component System** for database operations:
- **Tracker:** Attaches a `Dirty` component to entities when `PlayerState` changes.
- **Flusher:** Runs every 100 ticks, serializing dirty entities to Sled with Gzip compression and removing the marker.

### Startup Diagnostics

Kraken prints Paper-style diagnostic lines on boot:

- **Host OS:** OS family and architecture (e.g., `windows (x86_64)`).
- **Runtime:** Rust compiler version.
- **Memory:** Total system memory and process heap usage in MB.
- **Library Mapping:** Explicit loading sequence (`ViaKraken bridge`, `PersistencePlugin`, `WorldPlugin`, etc.).

## 📁 Generated Files

| File | Purpose |
|------|---------|
| `eula.txt` | Mojang EULA acceptance flag. Must be `eula=true` to boot. |
| `server.properties` | Bind IP, port, target protocol, max players, MOTD. |
| `world_data/` | Sled database directory for player and chunk persistence. |

## 📜 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

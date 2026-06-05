<div align="center">
  <h1>🦑 Kraken Minecraft Server</h1>
  <p><strong>High-performance Minecraft 1.21.11 / 26.1 server engine utilizing SIMD optimizations and an ECS architecture.</strong></p>
</div>

---

## ✨ Features

- 🦀 **Core:** Built on **Rust Nightly** for `portable_simd` optimizations, ensuring maximum throughput and minimal latency.
- ⚙️ **Network Engine:** Powered by **[Azalea](https://github.com/mat-1/azalea)** (1.21.1 Support), providing native protocol, block, entity, and inventory representation.
- 🧩 **Architecture:** Fully driven by **Bevy ECS**, enabling modular systems, robust plugin management, and an incredibly fast, highly concurrent game loop.
- 💾 **Database:** High-speed persistence via an embedded **Sled** key-value store, compressing binary chunk data and player states instantly using **Postcard + Flate2 (Gzip)**.
- 🗺️ **World Generation:** Custom **Perlin Noise** world generation with automatic `ChunkProvider` handling (e.g., solid Bedrock floor at Y=-64, Deepslate at Y=0).
- 🌉 **ViaKraken Bridge:** Included custom Bevy plugin intercepting connection packets and handshakes for seamless protocol mapping and fallback.

## 🚀 Getting Started

### Prerequisites

Kraken heavily utilizes `portable_simd` (required by dependencies like `simdnbt`) and therefore requires the **Rust Nightly** toolchain. The included `rust-toolchain.toml` should automatically handle this, but you can also run:

```bash
rustup toolchain install nightly
rustup override set nightly
```

### First Launch — EULA Gate

On the very first startup, Kraken performs an **EULA validation intercept** before initializing any network listeners, port bindings, or internal game loops.

- If `eula.txt` does **not** exist, the server will:
  1. Programmatically create `eula.txt` populated with `eula=false`.
  2. Programmatically create a default `server.properties` filled with native system defaults.
  3. Log a high-priority console alert that the EULA must be accepted.
  4. **Halt the runtime immediately** (`System.exit(0)` equivalent).

- If `eula.txt` exists but contains `eula=false`, the server will log the same alert and halt.

The engine **explicitly prevents binding to port 25565** or spinning up internal ECS game loops unless `eula=true` passes verification. Edit `eula.txt` to accept the Mojang EULA before restarting.

### Installation & Running

1. **Clone the repository:**
   ```bash
   git clone https://github.com/itarqos5/KrakenMC.git
   cd KrakenMC
   ```

2. **Build and run the server (Release mode recommended for performance):**
   ```bash
   cargo run --release
   ```

## 🖥️ Platform Notes

### Windows ANSI Console Colors

On Windows, Kraken detects stock Command Prompt environments and automatically forces **native ANSI escape-code parsing** via a Kernel32 `SetConsoleMode(handle, ENABLE_VIRTUAL_TERMINAL_PROCESSING)` call. This prevents broken text tokens and ensures clean color rendering even on legacy `cmd.exe` windows.

## 🏗️ Technical Architecture

### Protocol 26.1 (775) — LoginSuccess Trailing Boolean

Minecraft 26.1 appends a new trailing 1-byte Boolean flag immediately after the player UUID/Username payload in the `ClientboundLoginFinishedPacket` (Packet ID `0x02`). Kraken's stream decoder parser **explicitly consumes** this trailing boolean from the incoming ByteBuf stream when operating under Protocol 775, safely emptying the buffer channel and completely eliminating the `found N bytes extra` Netty DecoderException.

### Persistence Lifecycle

Kraken utilizes a **Dirty Component System** for blazing-fast database operations:
- **System A (Tracker):** Automatically attaches a `Dirty` component to any player or chunk entity whenever a position, inventory state, or block updates.
- **System B (Flusher):** Executes periodically (every 100 ticks), serializing all marked entities and cleanly writing payloads out to the `Sled` binary tree, dropping the `Dirty` marker post-action.

### Startup Diagnostics

Kraken replicates a premium Paper-style console environment. Upon boot, the following diagnostic lines are printed in sequence:

- **Host OS:** Operating system family and target architecture (e.g., `windows (x86_64)`).
- **Runtime:** Active Rust compiler version (e.g., `Rust 1.85.0`).
- **Memory:** Total system memory and current process heap usage in MB.
- **Library Mapping:** A detailed loading sequence showing exactly which system libraries are being mapped (`ViaKraken bridge`, `PersistencePlugin`, `WorldPlugin`, etc.).

## 📁 Generated Files

| File | Purpose |
|------|---------|
| `eula.txt` | Mojang End User License Agreement acceptance flag. Must be `eula=true` to boot. |
| `server.properties` | Core runtime configuration: bind IP, port, target protocol, max players, MOTD. |
| `world/` | Overworld region and chunk data (Sled KV store). |
| `world_nether/` | Nether dimension data. |
| `world_the_end/` | End dimension data. |

## 📜 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

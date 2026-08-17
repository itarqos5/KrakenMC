<div align="center">
  <h1>🦑 Kraken Minecraft Server</h1>
  <p><strong>Ultra-fast, sub-10ms startup Minecraft Java Edition proxy/server backend built in Rust with Bevy ECS and Pumpkin Protocol.</strong></p>
</div>

---

## Overview

Kraken is a high-performance Rust-based Minecraft server engine designed for low latency and high throughput. It features an ultra-fast asynchronous boot pipeline (starting in ~10ms), a protocol-aware proxy bridge (via the ViaKraken plugin), and a modular Bevy ECS backend utilizing **Pumpkin Protocol** primitives.

## ✨ Features

- ⚡ **Ultra-Fast Startup:** Asynchronous hardware diagnostic polling allows the server to fully initialize and bind to network ports in under **10ms**.
- 🦀 **Core:** Built on Rust with optimized memory management and concurrent task execution.
- ⚙️ **Network Engine:** Native Minecraft Java protocol implementation powered by **[Pumpkin Protocol](https://github.com/Pumpkin-MC/Pumpkin)** & `pumpkin-data`, supporting multi-version client connections (up to **1.21.11 / 26.1**).
- 🧩 **Architecture:** Driven by **Bevy ECS** with a cleanly modularized backend (`src/viakraken/java/backend/`) separated into dedicated state management, packet handlers, and session loops.
- ⚔️ **Combat & PvP:** Real-time entity interactions including 1-heart melee punch damage, realistic fall damage mechanics, hurt animations (`CEntityStatus`), and broadcasted sound effects (`EntityPlayerHurt`).
- 🔄 **Player Lifecycle:** Instant respawns, persistent inventory & health tracking, real-time multiplayer tablist synchronization, and interactive `/gamemode` autocompletion.
- 💾 **Persistence:** Embedded **Sled** storage for player state, compressed generated chunks, biome climate, and block modifications.
- 🌍 **World Generation:** Contextual biome borders, temperature, caves, depth-aware ores, and cross-chunk trees with on-demand chunk streaming.
- 👁️ **View Distance:** Honors each client's render-distance preference up to the configurable `view-distance` server limit (default 16).

## 🚀 Getting Started

### First Launch — EULA Gate

Before any network listeners or game loops start, Kraken validates the EULA:

- If `eula.txt` does **not** exist, the server creates it with `eula=false`, generates a default `server.properties`, logs an alert, and **halts immediately**.
- The engine **will not bind to port 25565** or initialize internal systems until `eula=true` is set. Edit `eula.txt` to accept the Mojang EULA before starting.

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

### Operators

Kraken creates an empty `ops.json` beside the executable on first login. Add operators using the standard Minecraft format, then reconnect the player:

```json
[
  {
    "uuid": "8667ba71-b85a-4004-af54-457a9734eed7",
    "name": "Player",
    "level": 4,
    "bypassesPlayerLimit": false
  }
]
```

Only operators receive in-game command permission and may use `/gamemode` or the game-mode switcher. Console `/op` and `/deop` changes apply immediately.

### Console Commands

Commands may be entered with or without the leading slash:

| Command | Description |
|---------|-------------|
| `/help` | Show console command usage. |
| `/list` | List online players. |
| `/op <player> [level]` | Grant an online player operator level 1–4. |
| `/deop <player>` | Remove operator status. |
| `/kill <player>` | Kill an online player. |
| `/gamemode <mode> <player>` | Change an online player's game mode. |

## 🏗️ Technical Architecture

### Modular Backend Structure (`src/viakraken/java/backend/`)
To prevent monolithic file bloat, the Java backend is divided into specialized modules:
- `mod.rs`: Manages TCP listener loops, status ping responses, and the initial login handshake state machine.
- `state.rs`: Thread-safe global registries (`HashMap<Uuid, OnlinePlayer>`) and tokio broadcast channels (`PlayerEvent`, chat, and block updates).
- `handler.rs`: Decodes incoming `ServerPacket` payloads (interact/attack, digging, block placement, chat commands) and processes combat physics.
- `play.rs`: Handles the active session loop, entity spawning/despawning, tablist updates, keep-alives, and chunk broadcasting.

### Startup Latency Optimization
By moving synchronous `sysinfo` hardware queries (OS memory and CPU enumeration) out of the critical bootstrap path and into a detached asynchronous thread, Kraken bypasses OS blocking calls during boot, consistently achieving start times around **10ms**.

## 📁 Generated Files

| File | Purpose |
|------|---------|
| `eula.txt` | Mojang EULA acceptance flag. Must be `eula=true` to boot. |
| `server.properties` | Bind IP, port, target protocol, max players, MOTD, and maximum view distance. |
| `ops.json` | Operator UUIDs, names, permission levels, and player-limit bypass settings. |
| `world_data/` | Sled database directory for player and chunk persistence. |

## 📜 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

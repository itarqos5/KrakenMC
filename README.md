<div align="center">
  <h1>🦑 Kraken Minecraft Server</h1>
  <p><strong>High-performance Minecraft 1.21.11 server engine utilizing SIMD optimizations and an ECS architecture.</strong></p>
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

## 🏗️ Technical Architecture

### Persistence Lifecycle
Kraken utilizes a **Dirty Component System** for blazing-fast database operations:
- **System A (Tracker):** Automatically attaches a `Dirty` component to any player or chunk entity whenever a position, inventory state, or block updates.
- **System B (Flusher):** Executes periodically (every 100 ticks), serializing all marked entities and cleanly writing payloads out to the `Sled` binary tree, dropping the `Dirty` marker post-action.

## 📜 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details. 
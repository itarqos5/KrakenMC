<div align="center">
  <h1>Kraken Server</h1>
  <p>A blazingly fast, multi-threaded Minecraft server & proxy built in Rust via Valence.</p>

  <img src="https://img.shields.io/badge/Language-Rust_Edition_2021-orange?style=for-the-badge&logo=rust" alt="Rust" />
  <img src="https://img.shields.io/badge/Engine-Valence-blue?style=for-the-badge&logo=rust" alt="Valence" />
  <img src="https://img.shields.io/badge/Database-Sled-green?style=for-the-badge&logo=database" alt="Sled DB" />
  <img src="https://img.shields.io/badge/Async-Tokio-yellow?style=for-the-badge&logo=rust" alt="Tokio" />
  <img src="https://img.shields.io/badge/Memory-mimalloc-red?style=for-the-badge&logo=c%2B%2B" alt="Mi-malloc" />
</div>

---

## ⚡ Overview
**Kraken** is an experimental high-performance server and proxy that handles seamless client handshake rewriting through an asynchronous `ViaKraken` pipeline natively powered by **[Tokio](https://tokio.rs/)**. Under the hood, it hooks securely into **[Valence](https://github.com/valence-rs/valence)**, leveraging Bevy's ECS to concurrently process chunks, handle packet transmission, manage entities, and provide an ultra-lightweight environment that doesn't buckle under load! 

## 🛠️ Features
- **Extremely Fast Procedural Multithreading**: Offloads chunk requests and procedural noise terrain calculations out of the main tick loop directly onto worker thread pools powered by memory-safe channels.
- **Embedded NoSQL Datastore via `sled`**: Blocks and world modifications are transparently persisted directly into an embedded K/V store `world_data` using compressed Postcard encoding via Delta chunks. Say goodbye to cumbersome block databases!
- **Zero-setup Offline Proxy**: Dynamically parses ping protocol logic to support Server List Ping connections out-of-the-box before seamlessly injecting clients locally.
- **Vanilla-friendly**: Provides familiar behavior like `DiggingState` breaking rules, block updates, Gamemode rules mapped to `EventReader`, and fully responsive Command Trees.

## 🚀 Running The Server
Getting Kraken up and running is as trivial as a single double click or a standard Cargo workflow.

### FOR DEVELOPERS:
```bash
cargo run --release
```
For Windows users, we provide a unified clean testing tool:
```bash
./test.bat
```
### FOR USERS
You will find a copy (EXE) file of the server in Workflows or Releases / On our website for the latest one

*(Automatically ensures optimal release builds, cleans cache dirs, and runs reliably)*

## 📦 Core Libraries
- **`valence & valence_command`** - Core Minecraft ECS parsing server library based on Bevy.
- **`sled`** - Blazingly fast embedded data-store for instant chunk/player persistence.
- **`mimalloc`** - Microsoft's performance-oriented memory allocator.
- **`tokio` & `flume`** - Async I/O networking backbone paired with extremely low latency channel messaging.
- **`postcard`** - A `no_std` focused serializer that compresses block data insanely tight.
- **`owo-colors`** - Fancy logging and string highlights.

## 🏗️ Architecture
Kraken fundamentally splits the workload into two domains:
1. **ViaKraken Proxy Node** - Listens to `0.0.0.0:25565`, performs variable length integer `VarInt` decoding, rewrites packet data safely on the fly (for protocol spoofing or disconnect handling during login), then proxies bidirectional streams asynchronously!
2. **Valence ECS Backbone** - Bound locally, processes all logical systems (block modifying, item use, entity ticking) synchronously. Heavy noise mappings and sled DB operations bypass the Bevy ECS by shipping their generation tasks directly over `flume` channels onto `chunk_workers`, returning populated uncompressed `UnloadedChunk` memory arrays completely latency-free back to the server!

## 🔧 Contributing
Feel free to fork the repository, clone the structure, and add features to `src/systems/mod.rs` integrating cleanly. Pull Requests perfectly aligned with ECS flow will be quickly reviewed.

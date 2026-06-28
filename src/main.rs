#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use bevy_app::App;
use bevy_ecs::prelude::*;
use owo_colors::OwoColorize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod config;
mod handlers;
mod logger;
mod systems;
mod viakraken;
mod world;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[derive(Resource, Clone)]
pub struct WorldDb(pub Arc<sled::Db>);

#[cfg(windows)]
unsafe fn enable_windows_vt_processing() -> bool {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };
    let handle = GetStdHandle(STD_OUTPUT_HANDLE);
    if handle == -1isize as _ {
        return false;
    }
    let mut mode = 0u32;
    if GetConsoleMode(handle, &mut mode) == 0 {
        return false;
    }
    mode |= ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    SetConsoleMode(handle, mode) != 0
}

fn print_startup_diagnostics() {
    use sysinfo::System;

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    logger::log_info!("Host OS: {} ({})", os, arch);

    let version = rustc_version_runtime::version();
    logger::log_info!(
        "Runtime: Rust {}.{}.{}",
        version.major, version.minor, version.patch
    );

    let mut sys = System::new();
    sys.refresh_memory();
    let total_mb = sys.total_memory() / 1024;
    let used_mb = sys.used_memory() / 1024;
    logger::log_info!("Memory: {} MB total / {} MB used", total_mb, used_mb);
}

fn main() {
    let start_time = std::time::Instant::now();
    // 1. Force-enable Windows Command Prompt ANSI VT processing via native Kernel32 call
    let windows_vt_enabled = unsafe {
        #[cfg(windows)]
        {
            enable_windows_vt_processing()
        }
        #[cfg(not(windows))]
        {
            false
        }
    };

    let color_supported = if cfg!(windows) {
        windows_vt_enabled
            || std::env::var("WT_SESSION").is_ok()
            || std::env::var("ANSICON").is_ok()
            || std::env::var("ConEmuANSI")
                .map(|v| v.eq_ignore_ascii_case("on"))
                .unwrap_or(false)
            || std::env::var("TERM")
                .map(|v| v.contains("xterm") || v.contains("ansi"))
                .unwrap_or(false)
    } else {
        true
    };
    owo_colors::set_override(color_supported);

    // 2. EULA gate: validate BEFORE any network listeners, DB init, or game loops
    let server_config = config::enforce_eula_gate();

    // 3. Paper-style startup diagnostics
    print_startup_diagnostics();

    ctrlc::set_handler(move || {
        SHUTDOWN.store(true, Ordering::SeqCst);
        std::process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");

    let db = world::initialize_world_db();
    println!(
        "{}",
        r#"
 _  _______         _  _______ _   _ 
| |/ /  __ \   /\  | |/ /  ___| \ | |
| ' /| |__) | /  \ | ' /| |__ |  \| |
|  < |  _  / / /\ \|  < |  __|| . ` |
| . \| | \ \/ ____ \ . \| |___| |\  |
|_|\_\_|  \_\/    \_\_|\_\____|_| \_|
"#
        .purple()
        .bold()
    );

    logger::log_info!(
        "Starting Kraken via Azalea infrastructure on {}:{}",
        server_config.server_ip,
        server_config.server_port
    );

    logger::log_info!("Mapping system library: ViaKraken bridge...");
    let vk_config = Arc::new(server_config.clone());
    logger::log_info!("Mapping system library: PersistencePlugin...");
    logger::log_info!("Mapping system library: WorldPlugin...");

    let mut app = App::new();

    app.insert_resource(WorldDb(db.clone()));

    app.add_plugins(viakraken::ViaKrakenPlugin { config: vk_config });
    app.add_plugins(systems::persistence::PersistencePlugin);
    app.add_plugins(world::WorldPlugin);

    let startup_ms = start_time.elapsed().as_millis();
    logger::log_info!("Done! Startup took {}ms", startup_ms);

    loop {
        app.update();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

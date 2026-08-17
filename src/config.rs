use std::fs;
use std::io::Write;

use crate::logger::{log_error, log_info, log_warn};

#[derive(Clone)]
pub struct ServerConfig {
    pub max_players: u32,
    pub server_ip: String,
    pub server_port: u16,
    pub target_protocol: i32,
    pub view_distance: i32,
    pub motd: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_players: 1000,
            server_ip: "0.0.0.0".to_string(),
            server_port: 25565,
            target_protocol: 776,
            view_distance: 16,
            motd: "Welcome to Kraken!".to_string(),
        }
    }
}

/// Enforces the EULA gate at the absolute top of bootstrap.
/// Returns the loaded ServerConfig only if eula=true.
/// Exits the process immediately if eula.txt is missing or eula=false,
/// preventing any network listeners or internal game loops from starting.
pub fn enforce_eula_gate() -> ServerConfig {
    let exe_dir = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    // Create world folders
    let _ = fs::create_dir_all(exe_dir.join("world"));
    let _ = fs::create_dir_all(exe_dir.join("world_nether"));
    let _ = fs::create_dir_all(exe_dir.join("world_the_end"));

    let eula_path = exe_dir.join("eula.txt");
    if !eula_path.exists() {
        log_warn!("eula.txt not found. Creating default...");
        let mut file = fs::File::create(&eula_path).unwrap();
        writeln!(file, "# By changing the setting below to TRUE you are indicating your agreement to our EULA (https://account.mojang.com/documents/minecraft_eula).").unwrap();
        writeln!(file, "eula=false").unwrap();

        let props_path = exe_dir.join("server.properties");
        if !props_path.exists() {
            log_warn!("server.properties not found. Generating default...");
            let mut file = fs::File::create(&props_path).unwrap();
            let defaults = ServerConfig::default();
            writeln!(file, "# Kraken Server Properties").unwrap();
            writeln!(file, "# Protocol Versions map:").unwrap();
            writeln!(file, "# 763 = 1.20.1").unwrap();
            writeln!(file, "# 764 = 1.20.2").unwrap();
            writeln!(file, "# 765 = 1.20.3").unwrap();
            writeln!(file, "# 766 = 1.20.4").unwrap();
            writeln!(file, "# 767 = 1.20.5 / 1.20.6").unwrap();
            writeln!(file, "# 776 = 1.21.11").unwrap();
            writeln!(file, "server-ip={}", defaults.server_ip).unwrap();
            writeln!(file, "server-port={}", defaults.server_port).unwrap();
            writeln!(file, "target-protocol={}", defaults.target_protocol).unwrap();
            writeln!(file, "max-players={}", defaults.max_players).unwrap();
            writeln!(file, "view-distance={}", defaults.view_distance).unwrap();
            writeln!(file, "motd={}", defaults.motd).unwrap();
            log_info!("Created server.properties");
        }

        log_error!("You must accept the EULA to run this server. Set eula=true in eula.txt.");
        std::process::exit(0);
    }

    let contents = fs::read_to_string(&eula_path).unwrap();
    let mut eula_accepted = false;
    for line in contents.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(val) = line.strip_prefix("eula=") {
            eula_accepted = val.trim().eq_ignore_ascii_case("true");
        }
    }

    if !eula_accepted {
        log_error!("You must accept the EULA to run this server. Set eula=true in eula.txt.");
        std::process::exit(0);
    }

    let mut config = ServerConfig::default();
    let props_path = exe_dir.join("server.properties");
    if !props_path.exists() {
        log_warn!("server.properties not found. Generating default...");
        let mut file = fs::File::create(&props_path).unwrap();
        writeln!(file, "# Kraken Server Properties").unwrap();
        writeln!(file, "# Protocol Versions map:").unwrap();
        writeln!(file, "# 763 = 1.20.1").unwrap();
        writeln!(file, "# 764 = 1.20.2").unwrap();
        writeln!(file, "# 765 = 1.20.3").unwrap();
        writeln!(file, "# 766 = 1.20.4").unwrap();
        writeln!(file, "# 767 = 1.20.5 / 1.20.6").unwrap();
        writeln!(file, "# 776 = 1.21.11").unwrap();
        writeln!(file, "server-ip={}", config.server_ip).unwrap();
        writeln!(file, "server-port={}", config.server_port).unwrap();
        writeln!(file, "target-protocol={}", config.target_protocol).unwrap();
        writeln!(file, "max-players={}", config.max_players).unwrap();
        writeln!(file, "view-distance={}", config.view_distance).unwrap();
        writeln!(file, "motd={}", config.motd).unwrap();
        log_info!("Created server.properties");
    } else {
        let contents = fs::read_to_string(&props_path).unwrap();
        for line in contents.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let mut parts = line.splitn(2, '=');
            if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                match k.trim() {
                    "server-ip" => config.server_ip = v.trim().to_string(),
                    "server-port" => config.server_port = v.trim().parse().unwrap_or(25565),
                    "target-protocol" => config.target_protocol = v.trim().parse().unwrap_or(776),
                    "max-players" => config.max_players = v.trim().parse().unwrap_or(1000),
                    "view-distance" => {
                        config.view_distance = v.trim().parse::<i32>().unwrap_or(16).clamp(2, 32)
                    }
                    "motd" => config.motd = v.trim().to_string(),
                    _ => {}
                }
            }
        }
    }

    config
}

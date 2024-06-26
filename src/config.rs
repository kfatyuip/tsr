use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::{env::current_dir, fs, net::IpAddr, path::PathBuf, sync::Mutex};

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub bind: BindConfig,
    pub server: ServerConfig,
    pub allowlist: Option<Vec<IpAddr>>,
    pub blocklist: Option<Vec<IpAddr>>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct BindConfig {
    pub addr: String,
    pub listen: i32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ServerConfig {
    pub info: String,
    pub root: PathBuf,
    pub auto_index: Option<bool>,
    pub index: Option<PathBuf>,
    pub error_page: Option<PathBuf>,
}

lazy_static! {
    pub static ref CONFIG_PATH: Mutex<String> = Mutex::new("config.yaml".to_owned());
    pub static ref CONFIG: Config = init_config();
}

fn init_config() -> Config {
    let config_path = CONFIG_PATH.lock().unwrap();
    match fs::read_to_string(config_path.to_owned()) {
        Ok(conf) => serde_yaml::from_str(&conf).unwrap(),
        _ => Config {
            bind: BindConfig {
                addr: "0.0.0.0".to_owned(),
                listen: 8080,
            },
            server: ServerConfig {
                info: "Powered by Rust".to_owned(),
                root: current_dir().unwrap(),
                auto_index: Some(false),
                index: None,
                error_page: Some("404.html".to_owned().into()),
            },
            allowlist: None,
            blocklist: None,
        },
    }
}

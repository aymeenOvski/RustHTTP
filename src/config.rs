//! Owner: Person B — parsing + validation.
//! The STRUCTS below are SHARED CONTRACT.
use std::collections::HashMap;
use std::path::PathBuf;

use crate::http::Method;

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    pub host: String,
    pub ports: Vec<u16>,
    pub server_names: Vec<String>,
    pub is_default: bool,
    pub client_max_body_size: usize,
    pub error_pages: HashMap<u16, PathBuf>,
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct RouteConfig {
    pub path: String,
    pub methods: Vec<Method>,
    pub root: Option<PathBuf>,
    pub redirect: Option<String>,
    pub index: Option<String>,
    pub autoindex: bool,
    /// extension (e.g. ".py") -> interpreter path
    pub cgi: HashMap<String, PathBuf>,
    pub upload_dir: Option<PathBuf>,
}

pub fn load(path: &str) -> Result<Config, String> {
    let _ = path; // TODO(B): read file, parse, validate
    Ok(dev_config())
}

/// Hardcoded config so everyone can work before the parser exists.
pub fn dev_config() -> Config {
    Config {
        servers: vec![ServerConfig {
            host: "127.0.0.1".into(),
            ports: vec![8080],
            is_default: true,
            client_max_body_size: 1_000_000,
            routes: vec![RouteConfig {
                path: "/".into(),
                methods: vec![Method::Get, Method::Post, Method::Delete],
                root: Some("./www".into()),
                index: Some("index.html".into()),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

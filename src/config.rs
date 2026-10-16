//! Owner: Person B — parsing + validation.
//! The STRUCTS below are SHARED CONTRACT.
use std::collections::HashMap;
use std::path::PathBuf;
use std::fs;

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

// Loads, parses, and validates our custom section-based config file from disk.
pub fn load(path: &str) -> Result<Config, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read config file '{}': {}", path, e))?;

    let mut config = Config::default();
    let mut current_server: Option<ServerConfig> = None;
    let mut current_route: Option<RouteConfig> = None;
    let mut section_type = "";

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Handle section headers like [server.main] or [route.main./]
        if line.starts_with('[') && line.ends_with(']') {
            // First, if we had an active route, push it to the current server
            if let Some(route) = current_route.take() {
                if let Some(ref mut s) = current_server {
                    s.routes.push(route);
                }
            }

            let header = &line[1..line.len() - 1].trim();
            let parts: Vec<&str> = header.splitn(3, '.').collect();

            if parts[0] == "server" && parts.len() == 2 {
                if let Some(s) = current_server.take() {
                    config.servers.push(s);
                }
                section_type = "server";
                current_server = Some(ServerConfig {
                    client_max_body_size: 1_000_000,
                    ..Default::default()
                });
            } else if parts[0] == "route" && parts.len() >= 3 {
                section_type = "route";
                current_route = Some(RouteConfig {
                    path: parts[2].to_string(),
                    ..Default::default()
                });
            }
            continue;
        }

        // Handle key = value pairs
        if let Some(eq) = line.find('=') {
            let key = line[..eq].trim();
            let val = line[eq + 1..].trim();

            if section_type == "server" {
                if let Some(ref mut s) = current_server {
                    match key {
                        "host" => s.host = val.to_string(),
                        "ports" => {
                            s.ports = val.split(',')
                                .map(|p| p.trim().parse::<u16>().unwrap_or(80))
                                .collect();
                        }
                        "server_names" => {
                            s.server_names = val.split(',')
                                .map(|n| n.trim().to_string())
                                .collect();
                        }
                        "is_default" => s.is_default = val.parse().unwrap_or(false),
                        "client_max_body_size" => s.client_max_body_size = val.parse().unwrap_or(1_000_000),
                        k if k.starts_with("error_") => {
                            if let Ok(code) = k.trim_start_matches("error_").parse::<u16>() {
                                s.error_pages.insert(code, PathBuf::from(val));
                            }
                        }
                        _ => {}
                    }
                }
            } else if section_type == "route" {
                if let Some(ref mut r) = current_route {
                    match key {
                        "root" => r.root = Some(PathBuf::from(val)),
                        "methods" => {
                            r.methods = val.split(',')
                                .filter_map(|m| Method::parse(m.trim()))
                                .collect();
                        }
                        "index" => r.index = Some(val.to_string()),
                        "autoindex" => r.autoindex = val.parse().unwrap_or(false),
                        "redirect" => r.redirect = Some(val.to_string()),
                        "upload_dir" => r.upload_dir = Some(PathBuf::from(val)),
                        "cgi_extension" => {
                            r.cgi.insert(val.to_string(), PathBuf::from("/usr/bin/python3"));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if let Some(route) = current_route.take() {
        if let Some(ref mut s) = current_server {
            s.routes.push(route);
        }
    }
    if let Some(s) = current_server.take() {
        config.servers.push(s);
    }

    validate(&config)?;
    Ok(config)
}

// Startup validation to catch duplicate ports and configuration errors.
pub fn validate(config: &Config) -> Result<(), String> {
    let mut seen_ports = std::collections::HashSet::new();

    for server in &config.servers {
        for port in &server.ports {
            if !seen_ports.insert(*port) {
                return Err(format!("Configuration Error: Port {} is configured multiple times across servers!", port));
            }
        }
    }

    if config.servers.is_empty() {
        return Err("Configuration Error: No servers defined in configuration file!".to_string());
    }

    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_duplicate_port_validation() {
        let mut config = Config::default();
        config.servers.push(ServerConfig {
            ports: vec![8080],
            ..Default::default()
        });
        config.servers.push(ServerConfig {
            ports: vec![8080], // Duplicate!
            ..Default::default()
        });

        let result = validate(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("configured multiple times"));
    }

    #[test]
    fn test_config_file_parsing() {
        // Create a temporary test config file
        let test_file = "test_config.conf";
        let config_content = r#"
            [server.test]
            host = 127.0.0.1
            ports = 7070
            is_default = true
            client_max_body_size = 2000000
            error_404 = www/errors/404.html

            [route.test./]
            root = www
            methods = GET, POST
            index = index.html
            autoindex = false
        "#;
        
        fs::write(test_file, config_content).expect("Failed to write temp test config");

        // Test loading it
        let result = load(test_file);
        
        // Clean up temp file regardless of test outcome
        let _ = fs::remove_file(test_file);

        assert!(result.is_ok(), "Config load failed: {:?}", result.err());
        let config = result.unwrap();
        
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host, "127.0.0.1");
        assert_eq!(config.servers[0].ports, vec![7070]);
        assert!(config.servers[0].is_default);
        assert_eq!(config.servers[0].client_max_body_size, 2_000_000);
        assert_eq!(config.servers[0].routes.len(), 1);
        assert_eq!(config.servers[0].routes[0].path, "/");
        assert_eq!(config.servers[0].routes[0].methods.len(), 2);
    }
}
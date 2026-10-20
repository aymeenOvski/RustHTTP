//! Owner: Person B — parsing + validation.
//! The STRUCTS below are SHARED CONTRACT.
use std::collections::HashMap;
use std::fs;
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
            if let Some(route) = current_route.take() {
                if let Some(ref mut s) = current_server {
                    s.routes.push(route);
                }
            }

            let header = &line[1..line.len() - 1].trim();

            let parts: Vec<&str> = header.splitn(3, '.').collect();

            if parts.get(0) == Some(&"server") && parts.len() == 2 {
                if let Some(s) = current_server.take() {
                    config.servers.push(s);
                }
                section_type = "server";
                current_server = Some(ServerConfig {
                    client_max_body_size: 1_000_000,
                    ..Default::default()
                });
            } else if parts.get(0) == Some(&"route") && parts.len() >= 3 {
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
                            s.ports = val
                                .split(',')
                                .map(|p| p.trim().parse::<u16>().unwrap_or(8080))
                                .collect();
                        }
                        "server_names" => {
                            s.server_names = val.split(',').map(|n| n.trim().to_string()).collect();
                        }
                        "is_default" => s.is_default = val.parse().unwrap_or(false),
                        "client_max_body_size" => {
                            s.client_max_body_size = val.parse().unwrap_or(1_000_000)
                        }
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
                            r.methods = val
                                .split(',')
                                .filter_map(|m| Method::parse(m.trim()))
                                .collect();
                        }
                        "index" => r.index = Some(val.to_string()),
                        "autoindex" => r.autoindex = val.parse().unwrap_or(false),
                        "redirect" => r.redirect = Some(val.to_string()),
                        "upload_dir" => r.upload_dir = Some(PathBuf::from(val)),
                        "cgi_extension" => {
                            r.cgi
                                .insert(val.to_string(), PathBuf::from("/usr/bin/python3"));
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
                return Err(format!(
                    "Configuration Error: Port {} is configured multiple times across servers!",
                    port
                ));
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
    /// Loads `text` as a config file. Each test uses its own file, because
    /// tests run at the same time.
    fn load_text(name: &str, text: &str) -> Result<Config, String> {
        let path =
            std::env::temp_dir().join(format!("localserver_{}_{name}.conf", std::process::id()));
        fs::write(&path, text).expect("cannot write the temp config");
        let result = load(path.to_str().unwrap());
        let _ = fs::remove_file(&path);
        result
    }

    /// One valid server block. `extra` lines go in the [server] section.
    /// The root is "src", which always exists when `cargo test` runs.
    fn server(name: &str, ports: &str, extra: &str) -> String {
        format!(
            "[server.{name}]\nhost = 127.0.0.1\nports = {ports}\n{extra}\n\
             [route.{name}./]\nroot = src\nmethods = GET\n\n"
        )
    }

    #[test]
    fn a_valid_config_loads() {
        assert!(load_text("valid", &server("a", "8080", "")).is_ok());
    }

    #[test]
    fn virtual_hosts_on_one_port_are_allowed() {
        let text = server("a", "8080", "server_names = a.com")
            + &server("b", "8080", "server_names = b.com");
        let result = load_text("vhosts", &text);
        assert!(
            result.is_ok(),
            "two names on one port must be accepted: {result:?}"
        );
    }

    #[test]
    fn the_same_port_twice_in_one_server_is_refused() {
        assert!(load_text("twice", &server("a", "8080, 8080", "")).is_err());
    }

    #[test]
    fn the_same_port_with_the_same_name_is_refused() {
        let text = server("a", "8080", "server_names = a.com")
            + &server("b", "8080", "server_names = a.com");
        assert!(load_text("same_name", &text).is_err());
    }

    #[test]
    fn invalid_ports_are_refused() {
        for (i, port) in ["abc", "70000", "0", "-1"].iter().enumerate() {
            let result = load_text(&format!("port{i}"), &server("a", port, ""));
            assert!(result.is_err(), "ports = {port} must be refused");
        }
    }

    #[test]
    fn invalid_body_sizes_are_refused() {
        for (i, size) in ["-5", "abc", "99999999999999999999999"].iter().enumerate() {
            let extra = format!("client_max_body_size = {size}");
            let result = load_text(&format!("size{i}"), &server("a", "8080", &extra));
            assert!(
                result.is_err(),
                "client_max_body_size = {size} must be refused"
            );
        }
    }

    #[test]
    fn a_missing_root_folder_is_refused() {
        let text = "[server.a]\nhost = 127.0.0.1\nports = 8080\n\n\
                    [route.a./]\nroot = /no/such/folder\nmethods = GET\n";
        assert!(load_text("root", text).is_err());
    }

    #[test]
    fn a_missing_error_page_is_refused() {
        let text = server("a", "8080", "error_404 = /no/such/page.html");
        assert!(load_text("page", &text).is_err());
    }
}

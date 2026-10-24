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
    // A parse error found in this server's section. Makes `check_server` skip it.
    pub parse_error: Option<String>,
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
    // A parse error found in this route. Makes `check_server` skip the server.
    pub parse_error: Option<String>,
}

// Only "true" or "false" are accepted for yes/no settings.
fn parse_bool(val: &str) -> Result<bool, String> {
    match val {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("expected true or false, got '{}'", val)),
    }
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
        let mut line = line.trim();

        if let Some(idx) = line.find('#') {
            line = &line[..idx];
        }
        let line = line.trim();

        if line.is_empty() {
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
                        "ports" if val.is_empty() => {}
                        "ports" => {
                            let parsed = val
                                .split(',')
                                .map(|p| {
                                    p.trim()
                                        .parse::<u16>()
                                        .ok()
                                        .filter(|&n| n != 0)
                                        .ok_or_else(|| format!("Invalid port '{}'", p.trim()))
                                })
                                .collect::<Result<Vec<u16>, String>>();
                            match parsed {
                                Ok(list) => s.ports = list,
                                Err(e) => s.parse_error = Some(e),
                            }
                        }
                        "server_names" => {
                            s.server_names = val.split(',').map(|n| n.trim().to_string()).collect();
                        }
                        "is_default" => match parse_bool(val) {
                            Ok(b) => s.is_default = b,
                            Err(e) => s.parse_error = Some(format!("is_default: {e}")),
                        },
                        "client_max_body_size" => match val.parse() {
                            Ok(n) => s.client_max_body_size = n,
                            Err(_) => {
                                s.parse_error =
                                    Some(format!("Invalid client_max_body_size '{}'", val))
                            }
                        },
                        k if k.starts_with("error_") => {
                            match k.trim_start_matches("error_").parse::<u16>() {
                                Ok(code) if (400..=599).contains(&code) => {
                                    s.error_pages.insert(code, PathBuf::from(val));
                                }
                                _ => {
                                    s.parse_error =
                                        Some(format!("invalid error page setting '{k}'"))
                                }
                            }
                        }
                        other => s.parse_error = Some(format!("unknown server setting '{other}'")),
                    }
                }
            } else if section_type == "route" {
                if let Some(ref mut r) = current_route {
                    match key {
                        "root" => r.root = Some(PathBuf::from(val)),
                        "methods" => {
                            r.methods.clear();
                            for m in val.split(',') {
                                match Method::parse(m.trim()) {
                                    Some(method) => r.methods.push(method),
                                    None => {
                                        r.parse_error = Some(format!(
                                            "unknown method '{}' (allowed: GET, POST, DELETE)",
                                            m.trim()
                                        ))
                                    }
                                }
                            }
                        }
                        "index" => r.index = Some(val.to_string()),
                        "autoindex" => match parse_bool(val) {
                            Ok(b) => r.autoindex = b,
                            Err(e) => r.parse_error = Some(format!("autoindex: {e}")),
                        },
                        "redirect" => r.redirect = Some(val.to_string()),
                        "upload_dir" => r.upload_dir = Some(PathBuf::from(val)),
                        "cgi_extension" if val.starts_with('.') => {
                            r.cgi
                                .insert(val.to_string(), PathBuf::from("/usr/bin/python3"));
                        }
                        "cgi_extension" => {
                            r.parse_error =
                                Some(format!("cgi_extension must start with '.', got '{val}'"))
                        }
                        other => r.parse_error = Some(format!("unknown route setting '{other}'")),
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

    apply_defaults(&mut config);

    // checks ONE server on its own. An error here only disables this server.
    let mut valid = Vec::new();
    for server in std::mem::take(&mut config.servers) {
        match check_server(&server) {
            Ok(()) => valid.push(server),
            Err(e) => eprintln!(
                "warning: server {:?} on {}:{:?} skipped: {}",
                server.server_names, server.host, server.ports, e
            ),
        }
    }
    config.servers = valid;

    validate(&config)?;
    Ok(config)
}

// Startup validation to catch duplicate ports and configuration errors.
// These are conflicts BETWEEN servers, so they stay fatal.
pub fn validate(config: &Config) -> Result<(), String> {
    if config.servers.is_empty() {
        return Err("Configuration Error: No servers defined in configuration file!".to_string());
    }

    // On one host:port, at most one server may be the default.
    let mut defaults: HashMap<(String, u16), usize> = HashMap::new();
    for server in &config.servers {
        if !server.is_default {
            continue;
        }
        for port in &server.ports {
            let count = defaults.entry((server.host.clone(), *port)).or_insert(0);
            *count += 1;
            if *count > 1 {
                return Err(format!(
                    "Configuration Error: more than one default server on {}:{}",
                    server.host, port
                ));
            }
        }
    }

    // Servers may share a port (virtual hosts), but not the same name on it.
    let mut seen = std::collections::HashSet::new();
    for server in &config.servers {
        // A server without names counts as one unnamed server.
        let names: Vec<String> = if server.server_names.is_empty() {
            vec![String::new()]
        } else {
            server
                .server_names
                .iter()
                .map(|n| n.to_ascii_lowercase())
                .collect()
        };
        for port in &server.ports {
            for name in &names {
                if !seen.insert((server.host.clone(), *port, name.clone())) {
                    return Err(format!(
                        "Configuration Error: Multiple servers are configured for the same host:port ({}:{}) and server name ({:?})!",
                        server.host, port, name
                    ));
                }
            }
        }
    }

    Ok(())
}

// checks ONE server on its own. An error here only disables this server.
fn check_server(server: &ServerConfig) -> Result<(), String> {
    if let Some(e) = &server.parse_error {
        return Err(e.clone());
    }

    if server.client_max_body_size == 0 {
        return Err("client_max_body_size must be a positive value".to_string());
    }

    // Checks this specific server for duplicate ports
    let mut ports_seen = std::collections::HashSet::new();
    for port in &server.ports {
        if !ports_seen.insert(*port) {
            return Err(format!(
                "Configuration Error: port {} is listed twice in the same server",
                port
            ));
        }
    }

    for (code, page) in &server.error_pages {
        if !page.is_file() {
            return Err(format!(
                "error page for {} not found: {}",
                code,
                page.display()
            ));
        }
    }
    for route in &server.routes {
        if let Some(e) = &route.parse_error {
            return Err(format!("route {}: {}", route.path, e));
        }
        if !route.path.starts_with('/') {
            return Err(format!("route path must start with '/': '{}'", route.path));
        }
        if route.root.is_none() && route.redirect.is_none() {
            return Err(format!("route {} needs a root or a redirect", route.path));
        }
        if let Some(root) = &route.root {
            if !root.is_dir() {
                return Err(format!("root folder not found: {}", root.display()));
            }
        }
    }
    Ok(())
}

// Fills in the values we can safely guess. Runs BEFORE validate.
fn apply_defaults(config: &mut Config) {
    for server in &mut config.servers {
        if server.host.is_empty() {
            server.host = "127.0.0.1".to_string();
        }
        if server.ports.is_empty() {
            server.ports = vec![8080];
        }
        for route in &mut server.routes {
            if route.methods.is_empty() {
                route.methods = vec![Method::Get];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn an_empty_ports_value_listens_on_8080() {
        let text = "[server.a]\nhost = 127.0.0.1\nports =\n\n[route.a./]\nroot = src\n";
        let config = load_text("emptyports", text).unwrap();
        assert_eq!(config.servers[0].ports, vec![8080]);
    }

    #[test]
    fn a_wrong_ports_value_is_still_refused() {
        let text = "[server.a]\nports = 8080,\n\n[route.a./]\nroot = src\n";
        assert!(load_text("trailingcomma", text).is_err());
    }

    #[test]
    fn missing_host_and_methods_get_defaults() {
        let text = "[server.a]\nports = 8080\n\n[route.a./]\nroot = src\n";
        let config = load_text("defaults", text).unwrap();
        assert_eq!(config.servers[0].host, "127.0.0.1");
        assert_eq!(config.servers[0].routes[0].methods, vec![Method::Get]);
    }

    #[test]
    fn a_route_path_without_slash_is_refused() {
        let text = "[server.a]\nports = 8080\n\n[route.a.docs]\nroot = src\n";
        assert!(load_text("badpath", text).is_err());
    }

    #[test]
    fn a_route_without_root_or_redirect_is_refused() {
        let text = "[server.a]\nports = 8080\n\n[route.a./]\nmethods = GET\n";
        assert!(load_text("noroot", text).is_err());
    }

    #[test]
    fn a_redirect_only_route_is_fine() {
        let text = "[server.a]\nports = 8080\n\n[route.a./old]\nredirect = /\n";
        assert!(load_text("redirect", text).is_ok());
    }

    #[test]
    fn a_server_without_ports_listens_on_8080() {
        let text = "[server.a]\nhost = 127.0.0.1\n\n[route.a./]\nroot = src\n";
        let config = load_text("noports", text).unwrap();
        assert_eq!(config.servers[0].ports, vec![8080]);
    }

    #[test]
    fn two_unnamed_servers_without_ports_clash_on_8080() {
        let text = "[server.a]\n\n[route.a./]\nroot = src\n\n\
                    [server.b]\n\n[route.b./]\nroot = src\n";
        assert!(load_text("noports_twice", text).is_err());
    }

    #[test]
    fn two_defaults_on_one_port_are_refused() {
        let text = server("a", "8080", "server_names = a.com\nis_default = true")
            + &server("b", "8080", "server_names = b.com\nis_default = true");
        let err = load_text("two_defaults", &text).unwrap_err();
        assert!(err.contains("default"), "{err}");
    }

    #[test]
    fn defaults_on_different_ports_are_fine() {
        let text = server("a", "8080", "server_names = a.com\nis_default = true")
            + &server("b", "9090", "server_names = b.com\nis_default = true");
        assert!(load_text("defaults_ok", &text).is_ok());
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
            error_404 = error_pages/404.html

            [route.test./]
            root = src
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

    // one broken server must not stop the others.
    #[test]
    fn a_broken_server_is_skipped_and_the_other_works() {
        let broken = "[server.bad]\nhost = 127.0.0.1\nports = 8080\nserver_names = bad.com\n\n\
                      [route.bad.]\nroot = src\n\n";
        let good = server("good", "8080", "server_names = good.com");
        let config = load_text("skip_bad", &(broken.to_string() + &good)).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].server_names, vec!["good.com"]);
    }

    #[test]
    fn a_server_with_a_bad_value_is_skipped_and_the_other_works() {
        let bad = server(
            "bad",
            "8080",
            "server_names = bad.com\nclient_max_body_size = asd",
        );
        let good = server("good", "8080", "server_names = good.com");
        let config = load_text("skip_parse", &(bad + &good)).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].server_names, vec!["good.com"]);
    }

    #[test]
    fn an_unknown_method_skips_that_server() {
        let bad = "[server.bad]\nports = 8080\nserver_names = bad.com\n\n\
                   [route.bad./]\nroot = src\nmethods = GET, TRASH\n\n";
        let good = server("good", "8080", "server_names = good.com");
        let config = load_text("trash", &(bad.to_string() + &good)).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].server_names, vec!["good.com"]);
    }

    #[test]
    fn yes_no_settings_only_accept_true_or_false() {
        let bad = "[server.bad]\nports = 8080\nserver_names = bad.com\n\n\
                   [route.bad./]\nroot = src\nautoindex = yes\n\n";
        let good = server("good", "8080", "server_names = good.com");
        let config = load_text("autoindex", &(bad.to_string() + &good)).unwrap();
        assert_eq!(config.servers.len(), 1);
    }

    #[test]
    fn an_unknown_setting_skips_that_server() {
        let bad = server("bad", "8080", "server_names = bad.com\nhots = x");
        let good = server("good", "8080", "server_names = good.com");
        let config = load_text("unknown_key", &(bad + &good)).unwrap();
        assert_eq!(config.servers.len(), 1);
    }
}

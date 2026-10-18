//! Owner: Person B
use std::collections::HashMap;

pub fn parse_cookies(header: &str) -> HashMap<String, String> {
    let mut cookies = HashMap::new();
    
    // cookie example: "session_id=abc123; theme=dark; logged_in=true"
    for pair in header.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        
        if let Some((key, value)) = pair.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            cookies.insert(key, value);
        }
    }
    
    cookies
}
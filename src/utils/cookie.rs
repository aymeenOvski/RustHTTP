//! Owner: Person B
use std::collections::HashMap;

pub fn parse_cookies(header: &str) -> HashMap<String, String> {
    let _ = header; // TODO(B): split the raw cookie header string by semicolons and equal signs to turn it into a lookup map.
    HashMap::new()
}

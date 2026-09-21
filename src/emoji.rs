use std::collections::HashMap;
use std::fs;

use regex::Regex;

/// Load the emoji map from a JSON file: `{ "token": "replacement", ... }`.
pub fn load_map(path: &str) -> HashMap<String, String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

/// Replace `:token:` and standalone `token` occurrences with their mapped value.
pub fn apply(map: &HashMap<String, String>, text: &str) -> String {
    let mut out = text.to_string();
    for (key, val) in map {
        let colon = format!(":{}:", key);
        out = out.replace(&colon, val);
    }
    for (key, val) in map {
        let pattern = format!(r"\b{}\b", regex::escape(key));
        if let Ok(re) = Regex::new(&pattern) {
            out = re.replace_all(&out, val.as_str()).to_string();
        }
    }
    out
}
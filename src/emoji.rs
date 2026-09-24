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

/// Precompile the word-boundary regex for every map key ONCE. `apply` runs on
/// the AuthVerify hot path, so compiling `\b<key>\b` per key per message (big
/// map + high chat rate) is a slow path toward the engine's probe window.
/// Rebuild this cache whenever the map is loaded/changed.
pub fn compile_regexes(map: &HashMap<String, String>) -> HashMap<String, Regex> {
    let mut out = HashMap::with_capacity(map.len());
    for key in map.keys() {
        if key.is_empty() {
            continue;
        }
        let pattern = format!(r"\b{}\b", regex::escape(key));
        if let Ok(re) = Regex::new(&pattern) {
            out.insert(key.clone(), re);
        }
    }
    out
}

/// Replace `:token:` and standalone `token` occurrences with their mapped value.
/// `regexes` must be `compile_regexes(map)`.
pub fn apply(
    map: &HashMap<String, String>,
    regexes: &HashMap<String, Regex>,
    text: &str,
) -> String {
    let mut out = text.to_string();
    for (key, val) in map {
        let colon = format!(":{}:", key);
        out = out.replace(&colon, val);
    }
    for (key, re) in regexes {
        if let Some(val) = map.get(key) {
            out = re.replace_all(&out, val.as_str()).to_string();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_replaces_colon_and_word_tokens() {
        let map = HashMap::from([
            ("kek".to_string(), "😂".to_string()),
            ("pepe".to_string(), "🐸".to_string()),
        ]);
        let regexes = compile_regexes(&map);
        let out = apply(&map, &regexes, "hello :kek: and pepe world");
        assert_eq!(out, "hello 😂 and 🐸 world");
    }

    #[test]
    fn word_boundary_prevents_substring_matches() {
        let map = HashMap::from([("cat".to_string(), "🐱".to_string())]);
        let regexes = compile_regexes(&map);
        // "cat" inside "scatter" must NOT be replaced.
        let out = apply(&map, &regexes, "scatter is a cat");
        assert_eq!(out, "scatter is a 🐱");
    }

    #[test]
    fn compile_skips_empty_keys() {
        let map = HashMap::from([("".to_string(), "x".to_string())]);
        let regexes = compile_regexes(&map);
        assert!(regexes.is_empty());
    }
}
//! Secure Token Storage management module - wraps the `keyring` crate
//! to store secret tokens (e.g. API keys) in the OS-level secure credential vault
//! (Windows Credential Manager, macOS Keychain, Linux Secret Service).
//! Automatically falls back to plain `AppConfig` when OS Keyring is unavailable.

use std::collections::HashMap;

const SERVICE_NAME: &str = "kotoshelf";
const LEGACY_SERVICE_NAME: &str = "kotoshelf-native";

/// Sets a token in the OS Keyring. Returns `true` if successful,
/// `false` if OS Keyring is unavailable or failed.
pub fn set_token(key: &str, value: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    match keyring::Entry::new(SERVICE_NAME, key) {
        Ok(entry) => entry.set_password(value).is_ok(),
        Err(_) => false,
    }
}

/// Retrieves a token from the OS Keyring. Returns `Some(value)` if found, `None` if missing or unavailable.
pub fn get_token(key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    if let Ok(entry) = keyring::Entry::new(SERVICE_NAME, key) {
        if let Ok(pw) = entry.get_password() {
            return Some(pw);
        }
    }
    // Migrate from legacy "kotoshelf-native" keyring service if present
    let legacy_entry = keyring::Entry::new(LEGACY_SERVICE_NAME, key).ok()?;
    let pw = legacy_entry.get_password().ok()?;
    let _ = set_token(key, &pw);
    let _ = legacy_entry.delete_credential();
    Some(pw)
}

/// Deletes a token from the OS Keyring.
pub fn delete_token(key: &str) {
    if key.is_empty() {
        return;
    }
    if let Ok(entry) = keyring::Entry::new(SERVICE_NAME, key) {
        let _ = entry.delete_credential();
    }
    if let Ok(legacy_entry) = keyring::Entry::new(LEGACY_SERVICE_NAME, key) {
        let _ = legacy_entry.delete_credential();
    }
}

/// Synchronizes tokens on application startup:
/// 1. Reads all tokens from OS Keyring where available.
/// 2. If a plain-text token exists in `config_tokens` but is NOT in Keyring, attempts to migrate it to Keyring.
/// 3. Returns resolved in-memory `HashMap<String, String>` and `need_config_save` flag (if migration succeeded and config needs cleanup).
pub fn sync_tokens_on_load(mut config_tokens: HashMap<String, String>) -> (HashMap<String, String>, bool) {
    let mut resolved = HashMap::new();
    let mut config_cleaned = false;

    for (key, plain_val) in config_tokens.drain() {
        if key.is_empty() {
            continue;
        }
        if let Some(secure_val) = get_token(&key) {
            resolved.insert(key, secure_val);
            if !plain_val.is_empty() {
                config_cleaned = true;
            }
        } else if !plain_val.is_empty() {
            // Migration: try saving to Keyring
            if set_token(&key, &plain_val) {
                resolved.insert(key, plain_val);
                config_cleaned = true;
            } else {
                // Keyring unsupported/failed: keep fallback in config
                resolved.insert(key, plain_val);
            }
        }
    }

    (resolved, config_cleaned)
}

/// Prepares tokens for saving:
/// Stores secrets in OS Keyring. Returns:
/// - `resolved_tokens`: In-memory full key-value map for immediate app use.
/// - `config_tokens`: Map to write to `AppConfig.tokens` (`""` for Keyring-saved items, full value for fallback items).
pub fn prepare_tokens_for_save(tokens: &HashMap<String, String>) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut resolved = HashMap::new();
    let mut config_tokens = HashMap::new();

    for (key, val) in tokens {
        if key.is_empty() {
            continue;
        }
        resolved.insert(key.clone(), val.clone());
        if set_token(key, val) {
            // Successfully stored in Keyring: store empty placeholder in JSON config
            config_tokens.insert(key.clone(), String::new());
        } else {
            // Keyring unavailable: fallback to config JSON
            config_tokens.insert(key.clone(), val.clone());
        }
    }

    (resolved, config_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_tokens_on_load_resolves_and_handles_migration() {
        let mut raw = HashMap::new();
        raw.insert("my_token".to_string(), "secret_val".to_string());

        let (resolved, _cleaned) = sync_tokens_on_load(raw);
        assert_eq!(resolved.get("my_token"), Some(&"secret_val".to_string()));

        // Cleanup test token from OS keyring
        delete_token("my_token");
    }
}

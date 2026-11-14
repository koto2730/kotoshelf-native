//! Light/Dark/System theme selection - ported from `src/lib/theme.ts` and
//! the `get_selected_theme`/`set_selected_theme` Tauri commands, scoped
//! down to the built-in Light/Dark/System choices (the original also
//! supports arbitrary user-authored themes at `~/.kotoshelf/themes/*.json`
//! - not ported, see `work/HANDOFF.md`). Persisted to the same
//! `~/.kotoshelf/theme.json` file/shape as the original
//! (`{"selected": "system"}`) so switching between kotoshelf and
//! kotoshelf-native doesn't reset the choice.

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeSelection {
    System,
    Light,
    Dark,
}

impl Default for ThemeSelection {
    fn default() -> Self {
        ThemeSelection::System
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ThemeFile {
    selected: ThemeSelection,
}

fn theme_path() -> Result<std::path::PathBuf, String> {
    let home = dirs::home_dir().ok_or("Could not determine home directory")?;
    Ok(home.join(".kotoshelf").join("theme.json"))
}

/// Falls back to `ThemeSelection::default()` (`System`) for a missing
/// file, unreadable JSON, or - deliberately, since custom theme names
/// aren't supported here - a `selected` value that isn't `"system"`,
/// `"light"`, or `"dark"` (e.g. a real kotoshelf custom theme name).
pub fn load_selected() -> ThemeSelection {
    theme_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<ThemeFile>(&s).ok())
        .map(|f| f.selected)
        .unwrap_or_default()
}

pub fn save_selected(selected: ThemeSelection) -> Result<(), String> {
    let path = theme_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(&ThemeFile { selected }).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("Failed to write {path:?}: {e}"))
}

/// Resolves a selection to an actual light/dark bool - `System` asks the
/// OS (`dark_light::detect()`), falling back to light if that fails
/// (matches `window.matchMedia` simply reporting "no preference" as
/// light in a browser). Called every frame from `app.rs` rather than
/// cached, so an OS-level theme flip is picked up live the same way the
/// original's `matchMedia` change listener does - cheap enough (a single
/// registry/API read) that there's no need for a dedicated poll timer.
pub fn resolve_dark(selection: ThemeSelection) -> bool {
    match selection {
        ThemeSelection::Light => false,
        ThemeSelection::Dark => true,
        ThemeSelection::System => matches!(dark_light::detect(), Ok(dark_light::Mode::Dark)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_dark_is_direct_for_explicit_choices() {
        assert!(!resolve_dark(ThemeSelection::Light));
        assert!(resolve_dark(ThemeSelection::Dark));
    }

    #[test]
    fn theme_selection_round_trips_through_json_in_the_original_shape() {
        let json = serde_json::to_string(&ThemeFile { selected: ThemeSelection::Dark }).unwrap();
        assert_eq!(json, r#"{"selected":"dark"}"#);
        let parsed: ThemeFile = serde_json::from_str(r#"{"selected":"system"}"#).unwrap();
        assert_eq!(parsed.selected, ThemeSelection::System);
    }

    #[test]
    fn an_unrecognized_custom_theme_name_fails_to_parse_rather_than_being_silently_misread() {
        // A real kotoshelf config with a custom theme selected - we don't
        // support those, so this should fail parsing (the caller falls
        // back to `System` via `unwrap_or_default`), not be silently
        // coerced into one of the three built-in variants.
        assert!(serde_json::from_str::<ThemeFile>(r#"{"selected":"my-custom-theme"}"#).is_err());
    }
}

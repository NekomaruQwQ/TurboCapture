//! Preview-owned selector preset parsing and window matching.
//!
//! The JSON and pattern syntax intentionally remains compatible with the
//! server and `live-capture`, while this implementation can evolve preview
//! selection behavior without changing the streaming selector.

use std::collections::HashMap;

/// Full server selector configuration with one active named preset.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PresetConfig {
    /// Name of the active entry in [`Self::presets`].
    pub preset: String,
    /// Named pattern lists supplied by `live-server`.
    pub presets: HashMap<String, Vec<String>>,
}

impl PresetConfig {
    /// Return the active pattern list, or `None` when the server references a
    /// missing preset. A missing preset deliberately produces no selection.
    pub fn active_patterns(&self) -> Option<&Vec<String>> { self.presets.get(&self.preset) }
}

/// Parsed `[@mode] <exePath>[@<windowTitle>]` selector pattern.
#[derive(Debug, Clone)]
pub struct ParsedPattern {
    /// Optional mode prefix retained for syntax compatibility and diagnostics.
    pub mode: Option<String>,
    /// Executable-path substring, with slash normalization during matching.
    pub exe_path: String,
    /// Optional case-insensitive window-title substring.
    pub title: Option<String>,
}

/// Parse one selector pattern without rejecting empty components.
///
/// Empty executable or title components behave as wildcards, matching the
/// established server configuration semantics. String indices come only from
/// ASCII delimiter searches and are therefore valid UTF-8 boundaries.
#[expect(clippy::string_slice, reason = "indices from str::find are valid UTF-8 boundaries")]
pub fn parse_pattern(pattern: &str) -> ParsedPattern {
    let mut mode: Option<String> = None;
    let mut body = pattern;

    if body.starts_with('@')
        && let Some(space_idx) = body.find(' ')
            && space_idx > 1 {
                mode = Some(body[1..space_idx].to_owned());
                body = &body[space_idx + 1..];
            }

    let (exe_path, title) = match body.find('@') {
        Some(idx) => (body[..idx].to_owned(), Some(body[idx + 1..].to_owned())),
        None => (body.to_owned(), None),
    };

    ParsedPattern { mode, exe_path, title }
}

/// Test one parsed pattern against executable path and window title.
///
/// `case_insensitive` is used by exclusion rules so casing cannot bypass a
/// veto. Include patterns preserve the existing case-sensitive path behavior;
/// title matching is always case-insensitive.
pub fn matches_parsed(
    parsed: &ParsedPattern,
    executable_path: &str,
    window_title: &str,
    case_insensitive: bool) -> bool {
    if !parsed.exe_path.is_empty() {
        let haystack = executable_path.replace('\\', "/");
        let needle = parsed.exe_path.replace('\\', "/");
        let matches = if case_insensitive {
            haystack.to_lowercase().contains(&needle.to_lowercase())
        } else {
            haystack.contains(&needle)
        };
        if !matches { return false; }
    }

    if let Some(ref title_pattern) = parsed.title
        && !title_pattern.is_empty()
            && !window_title.to_lowercase().contains(&title_pattern.to_lowercase()) {
            return false;
        }

    true
}

/// Determine whether a window should be previewed by the active patterns.
///
/// Exclusion matches veto the window regardless of pattern order. Otherwise,
/// any include match accepts the window. Mode annotations remain part of the
/// syntax because `exclude` uses the same prefix position, but previews do not
/// publish or otherwise consume include modes.
pub fn should_capture(
    patterns: &[String],
    executable_path: &str,
    title: &str) -> bool {
    let mut matched = false;

    for raw in patterns {
        let parsed = parse_pattern(raw);
        if parsed.mode.as_deref() == Some("exclude") {
            if matches_parsed(&parsed, executable_path, title, true) {
                return false;
            }
            continue;
        }
        if !matched && matches_parsed(&parsed, executable_path, title, false) {
            matched = true;
        }
    }

    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_patterns_returns_selected_preset() {
        let config = PresetConfig {
            preset: "main".into(),
            presets: HashMap::from([("main".into(), vec!["Code.exe".into()])]),
        };
        assert_eq!(config.active_patterns(), Some(&vec!["Code.exe".into()]));
    }

    #[test]
    fn parse_simple_executable() {
        let pattern = parse_pattern("devenv.exe");
        assert!(pattern.mode.is_none());
        assert_eq!(pattern.exe_path, "devenv.exe");
        assert!(pattern.title.is_none());
    }

    #[test]
    fn parse_mode_and_title() {
        let pattern = parse_pattern("@code Code.exe@LiveUI");
        assert_eq!(pattern.mode.as_deref(), Some("code"));
        assert_eq!(pattern.exe_path, "Code.exe");
        assert_eq!(pattern.title.as_deref(), Some("LiveUI"));
    }

    #[test]
    fn executable_matching_normalizes_path_separators() {
        let pattern = parse_pattern("C:/Program Files/JetBrains/");
        assert!(matches_parsed(
            &pattern,
            "C:\\Program Files\\JetBrains\\idea64.exe",
            "",
            false));
    }

    #[test]
    fn title_matching_is_case_insensitive() {
        let pattern = parse_pattern("Code.exe@liveui");
        assert!(matches_parsed(
            &pattern,
            "C:\\Code.exe",
            "Nekomaru LiveUI",
            false));
        assert!(!matches_parsed(
            &pattern,
            "C:\\Code.exe",
            "Some Other Window",
            false));
    }

    #[test]
    fn include_accepts_match_and_rejects_unmatched_window() {
        let patterns = vec!["@code devenv.exe".into()];
        assert!(should_capture(&patterns, "C:\\devenv.exe", "Test"));
        assert!(!should_capture(&patterns, "C:\\notepad.exe", "Test"));
    }

    #[test]
    fn exclusion_takes_priority_over_include() {
        let patterns = vec![
            "@game D:/7-Games/".into(),
            "@exclude vtube studio.exe".into(),
        ];
        assert!(!should_capture(
            &patterns,
            "D:/7-Games/vtube studio.exe",
            "VTube"));
    }
}

//! Selector config: pattern parsing and matching.
//!
//! Ported from `live-server/src/selector/config.rs`.  In M4 the config is
//! polled from the server via HTTP — no local file I/O.
//!
//! Pattern format: `[@mode] <exePath>[@<windowTitle>]`.

use std::collections::HashMap;

// ── Preset Config ───────────────────────────────────────────────────────────

/// Full config shape (matches server's JSON format).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PresetConfig {
    pub preset: String,
    pub presets: HashMap<String, Vec<String>>,
}

impl PresetConfig {
    /// Return the pattern list for the active preset, or `None` if missing.
    pub fn active_patterns(&self) -> Option<&Vec<String>> {
        self.presets.get(&self.preset)
    }
}

// ── Pattern Parsing ─────────────────────────────────────────────────────────

/// A parsed config pattern with optional mode tag and title filter.
#[derive(Debug, Clone)]
pub struct ParsedPattern {
    pub mode: Option<String>,
    pub exe_path: String,
    pub title: Option<String>,
}

/// Parse a config string: `[@mode] <exePath>[@<windowTitle>]`.
#[expect(clippy::string_slice, reason = "indices from str::find() are guaranteed valid UTF-8 boundaries")]
pub fn parse_pattern(pattern: &str) -> ParsedPattern {
    let mut mode: Option<String> = None;
    let mut body = pattern;

    // Extract leading `@mode ` prefix.
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

/// Test whether a window matches a parsed pattern.
pub fn matches_parsed(
    parsed: &ParsedPattern,
    executable_path: &str,
    window_title: &str,
    case_insensitive: bool,
) -> bool {
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
            && !window_title.to_lowercase().contains(&title_pattern.to_lowercase())
        {
            return false;
        }

    true
}

/// Result of pattern matching.
pub struct CaptureMatch {
    /// Mode tag from the matched include pattern (e.g. `"code"`, `"game"`).
    pub mode: Option<String>,
}

/// Determine whether a window should be captured based on the active preset.
///
/// Returns `None` if the window should not be captured.
pub fn should_capture(
    patterns: &[String],
    executable_path: &str,
    title: &str,
) -> Option<CaptureMatch> {
    let mut result: Option<CaptureMatch> = None;

    for raw in patterns {
        let parsed = parse_pattern(raw);
        if parsed.mode.as_deref() == Some("exclude") {
            if matches_parsed(&parsed, executable_path, title, true) {
                return None;
            }
        } else if result.is_none() && matches_parsed(&parsed, executable_path, title, false) {
            result = Some(CaptureMatch { mode: parsed.mode });
        } else {
            // Already matched or no match — skip.
        }
    }

    result
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_exe() {
        let p = parse_pattern("devenv.exe");
        assert!(p.mode.is_none());
        assert_eq!(p.exe_path, "devenv.exe");
        assert!(p.title.is_none());
    }

    #[test]
    fn parse_mode_prefix() {
        let p = parse_pattern("@code devenv.exe");
        assert_eq!(p.mode.as_deref(), Some("code"));
        assert_eq!(p.exe_path, "devenv.exe");
    }

    #[test]
    fn parse_exe_with_title() {
        let p = parse_pattern("@code Code.exe@LiveUI");
        assert_eq!(p.mode.as_deref(), Some("code"));
        assert_eq!(p.exe_path, "Code.exe");
        assert_eq!(p.title.as_deref(), Some("LiveUI"));
    }

    #[test]
    fn parse_exclude() {
        let p = parse_pattern("@exclude gogh.exe");
        assert_eq!(p.mode.as_deref(), Some("exclude"));
        assert_eq!(p.exe_path, "gogh.exe");
    }

    #[test]
    fn matches_exe_path_substring() {
        let p = parse_pattern("devenv.exe");
        assert!(matches_parsed(&p, "C:\\Program Files\\devenv.exe", "Window", false));
        assert!(!matches_parsed(&p, "C:\\Program Files\\code.exe", "Window", false));
    }

    #[test]
    fn matches_path_separator_normalization() {
        let p = parse_pattern("C:/Program Files/JetBrains/");
        assert!(matches_parsed(&p, "C:\\Program Files\\JetBrains\\idea64.exe", "", false));
    }

    #[test]
    fn matches_title_case_insensitive() {
        let p = parse_pattern("Code.exe@liveui");
        assert!(matches_parsed(&p, "C:\\Code.exe", "Nekomaru LiveUI", false));
        assert!(!matches_parsed(&p, "C:\\Code.exe", "Some Other Window", false));
    }

    #[test]
    fn should_capture_include_and_exclude() {
        let patterns = vec![
            "@code devenv.exe".into(),
            "@exclude gogh.exe".into(),
        ];
        let result = should_capture(&patterns, "C:\\devenv.exe", "Test");
        assert!(result.is_some());
        assert_eq!(result.unwrap().mode, Some("code".into()));

        assert!(should_capture(&patterns, "C:\\gogh.exe", "Test").is_none());
        assert!(should_capture(&patterns, "C:\\notepad.exe", "Test").is_none());
    }

    #[test]
    fn exclude_takes_priority() {
        let patterns = vec![
            "@game D:/7-Games/".into(),
            "@exclude vtube studio.exe".into(),
        ];
        assert!(should_capture(&patterns, "D:/7-Games/vtube studio.exe", "VTube").is_none());
    }
}

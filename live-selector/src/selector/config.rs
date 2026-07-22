//! Local selector-profile parsing, validation, and last-valid retention.
//!
//! The parsed policy stores normalized rules so foreground polling performs one
//! allocation per candidate path rather than repeatedly normalizing every rule.

use std::{
    collections::{BTreeSet, HashMap},
    fs::File,
    io::Read as _,
    path::Path,
};

use anyhow::Context as _;

/// Maximum accepted selector document size.
///
/// Profile files are expected to be tiny. Bounding the read prevents a damaged
/// or replaced file from causing an unbounded allocation before parsing.
const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// Deserialized top-level selector document.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectorDocument {
    /// Enabled profile names and their corresponding definitions.
    profiles: ProfileCollection,
}

/// Enabled profile names plus dynamically named profile definitions.
#[derive(Debug, serde::Deserialize)]
struct ProfileCollection {
    /// Only these named profiles participate in the active policy.
    enabled: Vec<String>,
    /// Every other key under `[profiles]` is a named profile definition.
    #[serde(flatten)]
    definitions: HashMap<String, ProfileDefinition>,
}

/// Executable-path substring rules contributed by one profile.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileDefinition {
    /// Allow rules contributed to the union when this profile is enabled.
    #[serde(default)]
    include: Vec<String>,
    /// Veto rules applied globally when this profile is enabled.
    #[serde(default)]
    exclude: Vec<String>,
}

/// Validated, normalized policy used by foreground matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorPolicy {
    /// Enabled profiles and their includes, retained in declared priority order.
    profiles: Vec<EnabledProfile>,
    /// Global vetoes contributed by every enabled profile.
    exclude_rules: Vec<String>,
}

/// One enabled profile retained so metadata can report the matching policy name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EnabledProfile {
    /// User-authored profile name used as the deterministic metadata label.
    name: String,
    /// Normalized executable-path includes contributed by this profile.
    include_rules: Vec<String>,
}

impl SelectorPolicy {
    /// Parse and validate one complete selector document.
    ///
    /// Unknown enabled profiles and empty rules are rejected because either can
    /// silently weaken a screen-sharing allowlist. No partially parsed policy is
    /// returned on error.
    pub fn parse(document: &str) -> anyhow::Result<Self> {
        let document: SelectorDocument =
            toml::from_str(document).context("invalid selector TOML")?;
        let unknown_profiles = document
            .profiles
            .enabled
            .iter()
            .filter(|name| !document.profiles.definitions.contains_key(*name))
            .collect::<BTreeSet<_>>();
        if !unknown_profiles.is_empty() {
            let names = unknown_profiles
                .into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("unknown enabled profiles: {names}");
        }

        let mut profiles = Vec::new();
        let mut exclude_rules = Vec::new();
        for profile_name in &document.profiles.enabled {
            // The unknown-profile check above proves every enabled name exists.
            let profile = document
                .profiles
                .definitions
                .get(profile_name)
                .expect("validated enabled profile must exist");
            let mut include_rules = Vec::new();
            append_rules(
                &mut include_rules,
                &profile.include,
                profile_name,
                "include")?;
            append_rules(
                &mut exclude_rules,
                &profile.exclude,
                profile_name,
                "exclude")?;
            profiles.push(EnabledProfile {
                name: profile_name.clone(),
                include_rules,
            });
        }

        Ok(Self {
            profiles,
            exclude_rules,
        })
    }

    /// Return the first enabled profile whose include accepts this executable.
    ///
    /// Global exclusions are evaluated first and veto every profile. Preserving
    /// the user-authored enabled order gives overlapping profiles a stable label
    /// without changing the existing union-based safety decision.
    pub fn matching_profile<'a>(&'a self, executable_path: &str) -> Option<&'a str> {
        let candidate = normalize_path(executable_path);
        if self
            .exclude_rules
            .iter()
            .any(|rule| candidate.contains(rule))
        {
            return None;
        }
        self.profiles
            .iter()
            .find(|profile| profile
                .include_rules
                .iter()
                .any(|rule| candidate.contains(rule)))
            .map(|profile| profile.name.as_str())
    }

    /// Decide whether an executable path is allowed by the active policy.
    ///
    /// Includes form a union, while any exclusion vetoes the candidate. An
    /// empty enabled set naturally has no include match and therefore fails
    /// closed.
    pub fn allows_executable(&self, executable_path: &str) -> bool {
        self.matching_profile(executable_path).is_some()
    }
}

/// Atomically replaceable policy state owned by the selector thread.
///
/// Candidate parsing happens before assignment, so a failed reload cannot
/// partially mutate or discard the last valid policy.
#[derive(Debug, Default)]
pub struct SelectorPolicyStore {
    /// Most recently accepted complete policy, or `None` before first success.
    active: Option<SelectorPolicy>,
}

impl SelectorPolicyStore {
    /// Return the active policy, if any configuration has succeeded.
    pub const fn active(&self) -> Option<&SelectorPolicy> {
        self.active.as_ref()
    }

    /// Load a bounded UTF-8 document and activate it only after validation.
    ///
    /// Returns whether the accepted policy differs from the previous one. File,
    /// size, UTF-8, syntax, and schema errors leave [`Self::active`] unchanged.
    pub fn reload_path(&mut self, path: &Path) -> anyhow::Result<bool> {
        let document = read_bounded_document(path)?;
        self.reload_document(&document)
    }

    /// Parse an in-memory candidate and activate it as one complete value.
    ///
    /// This boundary makes last-valid retention directly testable without
    /// coupling policy tests to filesystem timestamp behavior.
    fn reload_document(&mut self, document: &str) -> anyhow::Result<bool> {
        let candidate = SelectorPolicy::parse(document)?;
        let changed = self.active.as_ref() != Some(&candidate);
        self.active = Some(candidate);
        Ok(changed)
    }
}

/// Append normalized non-empty rules while preserving declaration order.
fn append_rules(
    destination: &mut Vec<String>,
    rules: &[String],
    profile_name: &str,
    rule_kind: &str) -> anyhow::Result<()> {
    for rule in rules {
        let normalized = normalize_path(rule.trim());
        if normalized.is_empty() {
            anyhow::bail!("profile \"{profile_name}\" contains an empty {rule_kind} rule");
        }
        if !destination.contains(&normalized) {
            destination.push(normalized);
        }
    }
    Ok(())
}

/// Normalize Windows path rules for slash- and case-insensitive matching.
fn normalize_path(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

/// Read at most [`MAX_CONFIG_BYTES`] plus one sentinel byte from a config file.
fn read_bounded_document(path: &Path) -> anyhow::Result<String> {
    let file = File::open(path)
        .with_context(|| format!("failed to open selector config {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read selector config {}", path.display()))?;
    if bytes.len() > MAX_CONFIG_BYTES {
        anyhow::bail!(
            "selector config {} exceeds the {} byte limit",
            path.display(),
            MAX_CONFIG_BYTES);
    }
    String::from_utf8(bytes)
        .with_context(|| format!("selector config {} is not UTF-8", path.display()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Representative policy used by parsing and retention tests.
    const CODE_POLICY: &str = r#"
[profiles]
enabled = ["code"]

[profiles.code]
include = ["Code.exe", "D:/Tools/Zed/"]
"#;

    #[test]
    fn empty_enabled_profiles_select_nothing() {
        let policy = SelectorPolicy::parse(
            r#"
[profiles]
enabled = []

[profiles.code]
include = ["Code.exe"]
"#).unwrap();
        assert!(!policy.allows_executable("C:/Apps/Code.exe"));
    }

    #[test]
    fn malformed_document_is_rejected() {
        let error = SelectorPolicy::parse("[profiles\nenabled = []")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid selector TOML"));
    }

    #[test]
    fn unknown_enabled_profiles_are_reported_by_name() {
        let error = SelectorPolicy::parse(
            r#"
[profiles]
enabled = ["missing", "also-missing"]

[profiles.code]
include = ["Code.exe"]
"#).unwrap_err()
        .to_string();
        assert!(error.contains("also-missing"));
        assert!(error.contains("missing"));
    }

    #[test]
    fn enabled_profile_includes_form_a_union() {
        let policy = SelectorPolicy::parse(
            r#"
[profiles]
enabled = ["code", "game"]

[profiles.code]
include = ["Code.exe"]

[profiles.game]
include = ["D:/Games/"]
"#).unwrap();
        assert!(policy.allows_executable("C:/Apps/Code.exe"));
        assert!(policy.allows_executable("D:/Games/example.exe"));
        assert!(!policy.allows_executable("C:/Windows/notepad.exe"));
    }

    #[test]
    fn exclusions_veto_includes_across_profiles() {
        let policy = SelectorPolicy::parse(
            r#"
[profiles]
enabled = ["games", "privacy"]

[profiles.games]
include = ["D:/Games/"]

[profiles.privacy]
exclude = ["D:/Games/unsafe-overlay.exe"]
"#).unwrap();
        assert!(policy.allows_executable("D:/Games/safe.exe"));
        assert!(!policy.allows_executable("D:/Games/unsafe-overlay.exe"));
    }

    #[test]
    fn matching_normalizes_slashes_and_case() {
        let policy = SelectorPolicy::parse(CODE_POLICY).unwrap();
        assert!(policy.allows_executable("c:\\apps\\CODE.EXE"));
        assert!(policy.allows_executable("d:\\tools\\zed\\Zed.exe"));
    }

    #[test]
    fn overlapping_profiles_report_first_enabled_name() {
        let policy = SelectorPolicy::parse(
            r#"
[profiles]
enabled = ["specific", "broad"]

[profiles.specific]
include = ["D:/Games/example.exe"]

[profiles.broad]
include = ["D:/Games/"]
"#).unwrap();
        assert_eq!(
            policy.matching_profile("d:\\games\\example.exe"),
            Some("specific"));
    }

    #[test]
    fn invalid_reload_retains_last_valid_policy() {
        let mut store = SelectorPolicyStore::default();
        assert!(store.reload_document(CODE_POLICY).unwrap());
        store.reload_document("[profiles").unwrap_err();
        assert!(
            store
                .active()
                .is_some_and(|policy| policy.allows_executable("C:/Apps/Code.exe")));
    }

    #[test]
    fn missing_initial_file_leaves_policy_inactive() {
        let missing = PathBuf::from(format!(
            "selector-config-that-does-not-exist-{}.toml",
            std::process::id()));
        let mut store = SelectorPolicyStore::default();
        store.reload_path(&missing).unwrap_err();
        assert!(store.active().is_none());
    }

    #[test]
    fn empty_rules_are_rejected() {
        let error = SelectorPolicy::parse(
            r#"
[profiles]
enabled = ["unsafe"]

[profiles.unsafe]
include = ["  "]
"#).unwrap_err()
        .to_string();
        assert!(error.contains("empty include rule"));
    }
}

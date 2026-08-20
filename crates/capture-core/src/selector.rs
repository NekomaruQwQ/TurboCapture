//! Pure deterministic target selection over platform-neutral window facts.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, btree_map::Entry},
    mem,
};

use serde::{Deserialize, Serialize};

use crate::config::{SelectionConfig, ValidationIssue};

/// Process-local identity assigned by the platform observation boundary.
///
/// The identifier is used only to correlate consecutive snapshots. It is not
/// exposed as a durable target identity by the instance status API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObservationId(pub u64);

/// Platform-neutral rectangle describing an observed window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBounds {
    /// Left edge in desktop coordinates.
    pub left: i32,
    /// Top edge in desktop coordinates.
    pub top: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Facts observed by a platform host without retaining any native object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedWindow {
    /// Process-local identity used for selection stickiness.
    pub id: ObservationId,
    /// Owning process identifier used only as a deterministic tie-breaker.
    pub process_id: u32,
    /// Executable file name available even when the full path cannot be read.
    pub executable_name: String,
    /// Full executable path when the platform permits observing it.
    pub executable_path: Option<String>,
    /// Current user-visible window title.
    pub title: String,
    /// Whether the platform currently reports the window as visible.
    pub visible: bool,
    /// Whether this is the platform's current foreground window.
    pub foreground: bool,
    /// Current desktop bounds used to reject zero-area observations.
    pub bounds: WindowBounds,
}

impl ObservedWindow {
    /// Returns the strongest executable fact available for policy matching.
    #[inline]
    fn executable_fact(&self) -> &str {
        self.executable_path.as_deref().unwrap_or(&self.executable_name)
    }
}

/// Validated normalized selection policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorPolicy {
    prefer_foreground: bool,
    profiles: Vec<SelectionProfile>,
    exclude_rules: Vec<String>,
}

/// One normalized profile retained in declared priority order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectionProfile {
    name: String,
    include_rules: Vec<String>,
}

/// A selected observation paired with the profile that admitted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection<'a> {
    /// Exact observation the platform host should translate to a native target.
    pub window: &'a ObservedWindow,
    /// First matching profile in configured priority order.
    pub profile: &'a str,
}

/// Pure transition chosen for one complete observation snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionDecision<'a> {
    /// Continue capturing the still-eligible current target.
    Keep(Selection<'a>),
    /// Replace an absent or ineligible target with this observation.
    Switch(Selection<'a>),
    /// No eligible target exists; the process remains alive and waiting.
    Wait,
}

/// Selects one deterministic target from an unordered observation snapshot.
///
/// A still-eligible current identity wins before ordinary ranking, preserving
/// target stability while focus changes. Otherwise profile order, configured
/// foreground preference, normalized facts, process ID, and observation ID
/// form a total deterministic order.
pub fn select_window<'a>(
    policy: &'a SelectorPolicy,
    observations: &'a [ObservedWindow],
    current: Option<ObservationId>) -> SelectionDecision<'a> {
    let candidates = observations
        .iter()
        .filter_map(|window| Candidate::new(policy, window))
        .collect::<Vec<_>>();

    if let Some(current) = current
        && let Some(candidate) = candidates
            .iter()
            .filter(|candidate| candidate.window.id == current)
            .min_by(|left, right| left.compare(right, policy.prefer_foreground))
    {
        return SelectionDecision::Keep(candidate.selection(policy));
    }

    candidates
        .iter()
        .min_by(|left, right| left.compare(right, policy.prefer_foreground))
        .map_or(SelectionDecision::Wait, |candidate| {
            SelectionDecision::Switch(candidate.selection(policy))
        })
}

/// One eligible observation with normalized facts used for total ordering.
struct Candidate<'a> {
    window: &'a ObservedWindow,
    profile_index: usize,
    normalized_executable: String,
    normalized_title: String,
}

impl<'a> Candidate<'a> {
    /// Filter and normalize one observation without retaining platform state.
    fn new(policy: &SelectorPolicy, window: &'a ObservedWindow) -> Option<Self> {
        if !window.visible || window.bounds.width == 0 || window.bounds.height == 0 {
            return None;
        }
        let normalized_executable = normalize(window.executable_fact());
        if policy
            .exclude_rules
            .iter()
            .any(|rule| normalized_executable.contains(rule))
        {
            return None;
        }
        let profile_index = policy
            .profiles
            .iter()
            .position(|profile| profile
                .include_rules
                .iter()
                .any(|rule| normalized_executable.contains(rule)))?;
        Some(Self {
            window,
            profile_index,
            normalized_executable,
            normalized_title: normalize(&window.title),
        })
    }

    /// Convert the internal ranked candidate into the public borrowed result.
    fn selection(&self, policy: &'a SelectorPolicy) -> Selection<'a> {
        Selection {
            window: self.window,
            profile: &policy.profiles[self.profile_index].name,
        }
    }

    /// Compare every stable ranking component in documented priority order.
    fn compare(&self, other: &Self, prefer_foreground: bool) -> Ordering {
        self.profile_index
            .cmp(&other.profile_index)
            .then_with(|| prefer_foreground
                .then_some(!self.window.foreground)
                .cmp(&prefer_foreground.then_some(!other.window.foreground)))
            .then_with(|| self.normalized_executable.cmp(&other.normalized_executable))
            .then_with(|| self.normalized_title.cmp(&other.normalized_title))
            .then_with(|| self.window.process_id.cmp(&other.window.process_id))
            .then_with(|| self.window.id.cmp(&other.window.id))
    }
}

/// Canonicalize user-authored rules and build the runtime policy on success.
pub(crate) fn validate_selection(
    config: &mut SelectionConfig,
    issues: &mut Vec<ValidationIssue>) -> SelectorPolicy {
    let mut canonical_profiles = BTreeMap::new();
    for (authored_name, mut profile) in mem::take(&mut config.profiles) {
        let name = authored_name.trim().to_owned();
        let profile_path = format!("selection.profiles[{authored_name:?}]");
        canonicalize_rules(
            &mut profile.include,
            &format!("{profile_path}.include"),
            issues);
        canonicalize_rules(
            &mut profile.exclude,
            &format!("{profile_path}.exclude"),
            issues);

        if name.is_empty() {
            issues.push(ValidationIssue::new(
                profile_path,
                "empty_profile_name",
                "profile name must not be empty"));
        } else {
            match canonical_profiles.entry(name) {
                Entry::Occupied(entry) => issues.push(ValidationIssue::new(
                    profile_path,
                    "duplicate_profile_name",
                    format!("canonical profile name {:?} is duplicated", entry.key()))),
                Entry::Vacant(entry) => {
                    entry.insert(profile);
                }
            }
        }
    }
    config.profiles = canonical_profiles;

    let mut profiles = Vec::new();
    let mut exclude_rules = Vec::new();
    let mut enabled_names = Vec::new();
    let mut all_includes = Vec::new();

    for (enabled_index, profile_name) in config.enabled.iter_mut().enumerate() {
        *profile_name = profile_name.trim().to_owned();
        let enabled_path = format!("selection.enabled[{enabled_index}]");
        if profile_name.is_empty() {
            issues.push(ValidationIssue::new(
                enabled_path,
                "empty_enabled_profile",
                "enabled profile name must not be empty"));
            continue;
        }
        if enabled_names.contains(profile_name) {
            issues.push(ValidationIssue::new(
                enabled_path,
                "duplicate_enabled_profile",
                format!("profile {profile_name:?} is enabled more than once")));
            continue;
        }
        enabled_names.push(profile_name.clone());

        let Some(profile) = config.profiles.get(profile_name) else {
            issues.push(ValidationIssue::new(
                enabled_path,
                "unknown_enabled_profile",
                format!("enabled profile {profile_name:?} is not defined")));
            continue;
        };
        let include_rules = profile.include.iter().map(|rule| normalize(rule)).collect::<Vec<_>>();
        let profile_excludes = profile.exclude.iter().map(|rule| normalize(rule));
        all_includes.extend(include_rules.iter().cloned());
        for rule in profile_excludes {
            if !exclude_rules.contains(&rule) {
                exclude_rules.push(rule);
            }
        }
        profiles.push(SelectionProfile {
            name: profile_name.clone(),
            include_rules,
        });
    }

    for rule in all_includes {
        if exclude_rules.contains(&rule) {
            issues.push(ValidationIssue::new(
                "selection.profiles",
                "contradictory_rule",
                format!("normalized rule {rule:?} is both included and excluded")));
        }
    }

    SelectorPolicy {
        prefer_foreground: config.prefer_foreground,
        profiles,
        exclude_rules,
    }
}

/// Trim and normalize one rule list while reporting empty and duplicate entries.
fn canonicalize_rules(
    source: &mut [String],
    path: &str,
    issues: &mut Vec<ValidationIssue>) {
    let mut normalized = Vec::new();
    for (index, rule) in source.iter_mut().enumerate() {
        *rule = rule.trim().to_owned();
        let normalized_rule = normalize(rule);
        if normalized_rule.is_empty() {
            issues.push(ValidationIssue::new(
                format!("{path}[{index}]"),
                "empty_selection_rule",
                "selection rules must not be empty"));
        } else if normalized.contains(&normalized_rule) {
            issues.push(ValidationIssue::new(
                format!("{path}[{index}]"),
                "duplicate_selection_rule",
                format!("normalized rule {normalized_rule:?} is duplicated")));
        } else {
            normalized.push(normalized_rule);
        }
    }
}

/// Normalize Windows executable facts for slash- and case-insensitive matching.
fn normalize(value: &str) -> String { value.replace('\\', "/").to_lowercase() }

#[cfg(test)]
mod tests {
    use crate::config::{
        InstanceConfig, RenderProfiles, SelectionProfileConfig, SourceConfig,
        VideoConfig,
    };

    use super::*;

    /// Build a policy whose enabled order is the public priority contract.
    fn policy(prefer_foreground: bool) -> SelectorPolicy {
        InstanceConfig {
            selection: SelectionConfig {
                prefer_foreground,
                enabled: vec!["code".to_owned(), "games".to_owned()],
                profiles: BTreeMap::from([
                    ("code".to_owned(), SelectionProfileConfig {
                        include: vec!["Code.exe".to_owned()],
                        exclude: vec![],
                    }),
                    ("games".to_owned(), SelectionProfileConfig {
                        include: vec!["D:/Games/".to_owned()],
                        exclude: vec!["private.exe".to_owned()],
                    }),
                ]),
            },
            source: SourceConfig::default(),
            video: VideoConfig {
                width: 1920,
                height: 1200,
                frame_rate: 60,
                bit_rate: 8_000_000,
            },
            render: RenderProfiles::default(),
        }
        .validate()
        .unwrap()
        .selector()
        .clone()
    }

    /// Construct one visible non-empty observation with concise call sites.
    fn window(id: u64, executable: &str, title: &str, foreground: bool) -> ObservedWindow {
        ObservedWindow {
            id: ObservationId(id),
            process_id: id as u32,
            executable_name: executable.to_owned(),
            executable_path: Some(format!("C:/Apps/{executable}")),
            title: title.to_owned(),
            visible: true,
            foreground,
            bounds: WindowBounds { left: 0, top: 0, width: 1280, height: 720 },
        }
    }

    #[test]
    fn selector_should_apply_global_exclusions() {
        let observations = [window(1, "private.exe", "Private", true)];
        assert_eq!(select_window(&policy(true), &observations, None), SelectionDecision::Wait);
    }

    #[test]
    fn selector_should_rank_profile_before_foreground_preference() {
        let policy = policy(true);
        let observations = [
            window(2, "game.exe", "Game", true),
            window(1, "Code.exe", "Code", false),
        ];
        let SelectionDecision::Switch(selection) =
            select_window(&policy, &observations, None)
        else {
            panic!("expected a selected target");
        };
        assert_eq!(selection.window.id, ObservationId(1));
    }

    #[test]
    fn selector_should_prefer_foreground_within_one_profile() {
        let policy = policy(true);
        let observations = [
            window(1, "Code.exe", "Alpha", false),
            window(2, "Code.exe", "Beta", true),
        ];
        let SelectionDecision::Switch(selection) =
            select_window(&policy, &observations, None)
        else {
            panic!("expected a selected target");
        };
        assert_eq!(selection.window.id, ObservationId(2));
    }

    #[test]
    fn selector_should_keep_a_still_eligible_current_target() {
        let policy = policy(true);
        let observations = [
            window(1, "Code.exe", "Current", false),
            window(2, "Code.exe", "Foreground", true),
        ];
        let SelectionDecision::Keep(selection) =
            select_window(&policy, &observations, Some(ObservationId(1)))
        else {
            panic!("expected sticky current target");
        };
        assert_eq!(selection.window.id, ObservationId(1));
    }

    #[test]
    fn selector_should_switch_after_current_target_disappears() {
        let policy = policy(false);
        let observations = [window(2, "Code.exe", "Replacement", false)];
        let SelectionDecision::Switch(selection) =
            select_window(&policy, &observations, Some(ObservationId(1)))
        else {
            panic!("expected replacement target");
        };
        assert_eq!(selection.window.id, ObservationId(2));
    }

    #[test]
    fn selector_should_ignore_snapshot_enumeration_order() {
        let policy = policy(false);
        let forward = [
            window(2, "Code.exe", "Beta", false),
            window(1, "Code.exe", "Alpha", false),
        ];
        let reverse = [forward[1].clone(), forward[0].clone()];

        let SelectionDecision::Switch(left) = select_window(&policy, &forward, None) else {
            panic!("expected first selection");
        };
        let SelectionDecision::Switch(right) = select_window(&policy, &reverse, None) else {
            panic!("expected second selection");
        };
        assert_eq!(left.window.id, right.window.id);
    }

    #[test]
    fn selector_should_wait_for_empty_or_non_visible_snapshots() {
        assert_eq!(select_window(&policy(false), &[], None), SelectionDecision::Wait);

        let mut hidden = window(1, "Code.exe", "Hidden", true);
        hidden.visible = false;
        assert_eq!(select_window(&policy(false), &[hidden], None), SelectionDecision::Wait);
    }

    #[test]
    fn validation_should_reject_definite_rule_contradictions() {
        let mut config = SelectionConfig {
            prefer_foreground: false,
            enabled: vec!["code".to_owned()],
            profiles: BTreeMap::from([(
                "code".to_owned(),
                SelectionProfileConfig {
                    include: vec!["Code.exe".to_owned()],
                    exclude: vec!["code.EXE".to_owned()],
                })]),
        };
        let mut issues = Vec::new();

        let _policy = validate_selection(&mut config, &mut issues);
        assert!(issues.iter().any(|issue| issue.code == "contradictory_rule"));
    }

    #[test]
    fn validation_should_reject_unknown_and_duplicate_enabled_profiles() {
        let mut config = SelectionConfig {
            prefer_foreground: false,
            enabled: vec!["missing".to_owned(), "code".to_owned(), " code ".to_owned()],
            profiles: BTreeMap::from([(
                "code".to_owned(),
                SelectionProfileConfig {
                    include: vec!["Code.exe".to_owned()],
                    exclude: vec![],
                })]),
        };
        let mut issues = Vec::new();

        let _policy = validate_selection(&mut config, &mut issues);

        assert!(issues.iter().any(|issue| issue.code == "unknown_enabled_profile"));
        assert!(issues.iter().any(|issue| issue.code == "duplicate_enabled_profile"));
    }
}

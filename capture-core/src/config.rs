//! Complete instance configuration, validation, and generation snapshots.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::selector::{SelectorPolicy, validate_selection};

/// Maximum accepted size of an initial TOML configuration document.
const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// Maximum number of simultaneous key colors supported by the browser shader.
pub const MAX_RENDER_KEYS: usize = 8;

/// One complete live capture-instance configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfig {
    /// Policy controlling which observed window owns the stream.
    pub selection: SelectionConfig,
    /// Source-region processing that may change without recreating the encoder.
    #[serde(default)]
    pub source: SourceConfig,
    /// Fixed encoded output settings that require a process restart to change.
    pub video: VideoConfig,
    /// Browser rendering parameters, optionally specialized by selected profile.
    #[serde(default)]
    pub render: RenderProfiles,
}

impl InstanceConfig {
    /// Validates and canonicalizes one complete candidate.
    ///
    /// Validation never returns a partially usable configuration. Profile
    /// names and matching rules are trimmed before the accepted value is stored.
    pub fn validate(mut self) -> Result<ValidatedInstanceConfig, ConfigError> {
        let mut issues = Vec::new();
        let selector = validate_selection(&mut self.selection, &mut issues);
        validate_source(&self.source, &mut issues);
        validate_video(&self.video, &mut issues);
        validate_render(&self.selection, &self.render, &mut issues);

        if !issues.is_empty() {
            return Err(ConfigError::Invalid { issues });
        }

        Ok(ValidatedInstanceConfig { config: self, selector })
    }
}

/// User-authored selection configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionConfig {
    /// Prefer the foreground candidate after profile priority is applied.
    #[serde(default)]
    pub prefer_foreground: bool,
    /// Enabled profiles in descending priority order.
    #[serde(default)]
    pub profiles: Vec<SelectionProfileConfig>,
}

/// One enabled selector profile and its executable-path rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionProfileConfig {
    /// Stable human-facing label included in status and render selection.
    pub name: String,
    /// Case-insensitive executable-path substrings accepted by this profile.
    #[serde(default)]
    pub include: Vec<String>,
    /// Global case-insensitive vetoes contributed by this profile.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Source processing that is independent of encoded output geometry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// Absolute captured-texture rectangle, or the complete texture when absent.
    #[serde(default)]
    pub crop: Option<CropRect>,
}

/// Absolute crop rectangle with exclusive maximum edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CropRect {
    /// Inclusive left edge in captured-texture pixels.
    pub min_x: u32,
    /// Inclusive top edge in captured-texture pixels.
    pub min_y: u32,
    /// Exclusive right edge in captured-texture pixels.
    pub max_x: u32,
    /// Exclusive bottom edge in captured-texture pixels.
    pub max_y: u32,
}

/// Fixed H.264 output settings owned by one process generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoConfig {
    /// Encoded width in pixels.
    pub width: u32,
    /// Encoded height in pixels.
    pub height: u32,
    /// Nominal encoded frame rate.
    pub frame_rate: u32,
    /// Target H.264 bitrate in bits per second.
    pub bit_rate: u32,
}

/// Default browser rendering parameters plus per-profile overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderProfiles {
    /// Rendering parameters used while waiting or without an override.
    #[serde(default)]
    pub default: RenderConfig,
    /// Complete rendering overrides keyed by selector profile name.
    #[serde(default)]
    pub profiles: BTreeMap<String, RenderConfig>,
}

impl RenderProfiles {
    /// Returns the complete render configuration for a selected profile.
    #[inline]
    pub fn for_profile(&self, profile: Option<&str>) -> &RenderConfig {
        profile
            .and_then(|name| self.profiles.get(name))
            .unwrap_or(&self.default)
    }
}

/// Browser color-key parameters sent independently of decoder configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderConfig {
    /// sRGB colors removed by the browser shader.
    #[serde(default)]
    pub key_colors: Vec<RgbColor>,
    /// Smoothstep range shaping the browser-generated alpha channel.
    #[serde(default)]
    pub color_key_knee: ColorKeyKnee,
    /// Optional constant sRGB color applied while preserving computed alpha.
    #[serde(default)]
    pub binarization_color: Option<RgbColor>,
}

/// One sRGB color represented exactly as three eight-bit channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RgbColor(pub [u8; 3]);

/// Smoothstep knee over the color-key alpha estimate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorKeyKnee {
    /// Values at or below this edge become fully transparent.
    pub low: f32,
    /// Values at or above this edge become fully opaque.
    pub high: f32,
}

impl Default for ColorKeyKnee {
    fn default() -> Self { Self { low: 0.02, high: 0.98 } }
}

/// A complete validated configuration plus normalized selector policy.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedInstanceConfig {
    config: InstanceConfig,
    selector: SelectorPolicy,
}

impl ValidatedInstanceConfig {
    /// Returns the canonical serializable configuration.
    #[inline]
    pub const fn config(&self) -> &InstanceConfig { &self.config }

    /// Returns the normalized pure selection policy.
    #[inline]
    pub const fn selector(&self) -> &SelectorPolicy { &self.selector }

    /// Validates a replacement against this process generation.
    ///
    /// Video media types remain fixed in M0, so a candidate changing them is
    /// rejected with an explicit restart boundary after semantic validation.
    pub fn validate_replacement(
        &self,
        candidate: InstanceConfig) -> Result<Self, ConfigError> {
        let candidate = candidate.validate()?;
        if candidate.config.video != self.config.video {
            return Err(ConfigError::RestartRequired {
                fields: vec!["video".to_owned()],
            });
        }
        Ok(candidate)
    }

    /// Returns browser parameters for a selected profile or the default.
    #[inline]
    pub fn render_for_profile(&self, profile: Option<&str>) -> &RenderConfig {
        self.config.render.for_profile(profile)
    }
}

/// One atomically published configuration generation.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigSnapshot {
    /// Monotonically increasing generation, beginning at one.
    pub generation: u64,
    /// Fully validated configuration visible to the media host.
    pub config: ValidatedInstanceConfig,
}

impl ConfigSnapshot {
    /// Constructs the initial generation for a validated startup configuration.
    #[inline]
    pub const fn initial(config: ValidatedInstanceConfig) -> Self {
        Self { generation: 1, config }
    }

    /// Produces the next accepted generation without mutating this snapshot.
    pub fn replaced(&self, config: ValidatedInstanceConfig) -> Result<Self, ConfigError> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(ConfigError::GenerationExhausted)?;
        Ok(Self { generation, config })
    }
}

/// One semantic validation failure with a stable machine-readable code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationIssue {
    /// Configuration path identifying the invalid value.
    pub path: String,
    /// Stable private API code for programmatic handling.
    pub code: &'static str,
    /// Human-readable explanation suitable for operator diagnostics.
    pub message: String,
}

impl ValidationIssue {
    /// Constructs one issue while keeping validation call sites compact.
    pub(crate) fn new(
        path: impl Into<String>,
        code: &'static str,
        message: impl Into<String>) -> Self {
        Self { path: path.into(), code, message: message.into() }
    }
}

/// Typed failures produced while loading, validating, or replacing configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The initial configuration file could not be opened or read.
    #[error("failed to read configuration {path}: {source}")]
    Io {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The bounded configuration document exceeded the accepted limit.
    #[error("configuration {path} exceeds the {limit} byte limit")]
    TooLarge {
        /// Oversized document path.
        path: PathBuf,
        /// Maximum permitted byte count.
        limit: usize,
    },
    /// The initial configuration document was not UTF-8.
    #[error("configuration {path} is not UTF-8: {source}")]
    InvalidUtf8 {
        /// Invalid document path.
        path: PathBuf,
        /// UTF-8 decoder diagnostic.
        #[source]
        source: std::string::FromUtf8Error,
    },
    /// TOML syntax or schema deserialization failed.
    #[error("invalid configuration TOML: {source}")]
    InvalidToml {
        /// TOML parser diagnostic.
        #[source]
        source: toml::de::Error,
    },
    /// One or more semantic invariants were rejected together.
    #[error("configuration failed semantic validation")]
    Invalid {
        /// Complete deterministic issue list for the candidate.
        issues: Vec<ValidationIssue>,
    },
    /// A valid candidate changed startup-only media settings.
    #[error("configuration fields require process restart: {fields:?}")]
    RestartRequired {
        /// Top-level fields that require a new process generation.
        fields: Vec<String>,
    },
    /// The generation counter cannot advance without losing monotonicity.
    #[error("configuration generation is exhausted")]
    GenerationExhausted,
}

/// Loads and validates a bounded UTF-8 TOML configuration document.
///
/// # Errors
///
/// Returns [`ConfigError`] for filesystem, size, UTF-8, TOML, or semantic
/// validation failures. No usable partial configuration is returned.
pub fn load_config(path: &Path) -> Result<ValidatedInstanceConfig, ConfigError> {
    let file = File::open(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            path: path.to_owned(),
            limit: MAX_CONFIG_BYTES,
        });
    }
    let document = String::from_utf8(bytes)
        .map_err(|source| ConfigError::InvalidUtf8 { path: path.to_owned(), source })?;
    let config = toml::from_str::<InstanceConfig>(&document)
        .map_err(|source| ConfigError::InvalidToml { source })?;
    config.validate()
}

/// Validate crop geometry before a platform host clamps it to a live texture.
fn validate_source(source: &SourceConfig, issues: &mut Vec<ValidationIssue>) {
    let Some(crop) = source.crop else { return };
    if crop.max_x <= crop.min_x {
        issues.push(ValidationIssue::new(
            "source.crop.max_x",
            "empty_crop_width",
            "max_x must be greater than min_x"));
    }
    if crop.max_y <= crop.min_y {
        issues.push(ValidationIssue::new(
            "source.crop.max_y",
            "empty_crop_height",
            "max_y must be greater than min_y"));
    }
}

/// Validate constraints shared by NV12, H.264, and the private wire format.
fn validate_video(video: &VideoConfig, issues: &mut Vec<ValidationIssue>) {
    for (path, value) in [("video.width", video.width), ("video.height", video.height)] {
        if value == 0 || value > u16::MAX.into() || value % 2 != 0 {
            issues.push(ValidationIssue::new(
                path,
                "invalid_video_dimension",
                "dimension must be a non-zero even value representable as u16"));
        }
    }
    if video.frame_rate == 0 {
        issues.push(ValidationIssue::new(
            "video.frame_rate",
            "zero_frame_rate",
            "frame_rate must be non-zero"));
    }
    if video.bit_rate == 0 {
        issues.push(ValidationIssue::new(
            "video.bit_rate",
            "zero_bit_rate",
            "bit_rate must be non-zero"));
    }
}

/// Validate renderer limits and ensure overrides name active selector profiles.
fn validate_render(
    selection: &SelectionConfig,
    render: &RenderProfiles,
    issues: &mut Vec<ValidationIssue>) {
    validate_render_config("render.default", &render.default, issues);

    let profile_names = selection
        .profiles
        .iter()
        .map(|profile| profile.name.as_str())
        .collect::<BTreeSet<_>>();
    for (profile_name, config) in &render.profiles {
        let path = format!("render.profiles.{profile_name}");
        if !profile_names.contains(profile_name.as_str()) {
            issues.push(ValidationIssue::new(
                &path,
                "unknown_render_profile",
                "render override must name an enabled selection profile"));
        }
        validate_render_config(&path, config, issues);
    }
}

/// Validate one complete set of browser shader parameters.
fn validate_render_config(
    path: &str,
    render: &RenderConfig,
    issues: &mut Vec<ValidationIssue>) {
    if render.key_colors.len() > MAX_RENDER_KEYS {
        issues.push(ValidationIssue::new(
            format!("{path}.key_colors"),
            "too_many_key_colors",
            format!("at most {MAX_RENDER_KEYS} key colors are supported")));
    }

    let knee = render.color_key_knee;
    if !knee.low.is_finite()
        || !knee.high.is_finite()
        || !(0.0..=1.0).contains(&knee.low)
        || !(0.0..=1.0).contains(&knee.high)
        || knee.low >= knee.high
    {
        issues.push(ValidationIssue::new(
            format!("{path}.color_key_knee"),
            "invalid_color_key_knee",
            "knee must contain finite 0 <= low < high <= 1 values"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small valid configuration used as a baseline for mutation tests.
    fn valid_config() -> InstanceConfig {
        InstanceConfig {
            selection: SelectionConfig {
                prefer_foreground: true,
                profiles: vec![SelectionProfileConfig {
                    name: "code".to_owned(),
                    include: vec!["Code.exe".to_owned()],
                    exclude: vec![],
                }],
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
    }

    #[test]
    fn validation_should_report_independent_failures_together() {
        let mut config = valid_config();
        config.source.crop = Some(CropRect { min_x: 5, min_y: 7, max_x: 5, max_y: 6 });
        config.video.width = 1;
        config.render.default.color_key_knee = ColorKeyKnee { low: 0.8, high: 0.2 };

        let ConfigError::Invalid { issues } = config.validate().unwrap_err() else {
            panic!("expected semantic validation failure");
        };
        assert_eq!(issues.len(), 4);
    }

    #[test]
    fn replacement_should_retain_restart_boundary_for_video_settings() {
        let current = valid_config().validate().unwrap();
        let mut candidate = valid_config();
        candidate.video.frame_rate = 30;

        let ConfigError::RestartRequired { fields } =
            current.validate_replacement(candidate).unwrap_err()
        else {
            panic!("expected restart-required failure");
        };
        assert_eq!(fields, ["video"]);
    }

    #[test]
    fn render_override_should_require_known_profile() {
        let mut config = valid_config();
        config.render.profiles.insert("missing".to_owned(), RenderConfig::default());

        let ConfigError::Invalid { issues } = config.validate().unwrap_err() else {
            panic!("expected semantic validation failure");
        };
        assert_eq!(issues[0].code, "unknown_render_profile");
    }

    #[test]
    fn snapshot_should_advance_only_after_accepted_replacement() {
        let current = ConfigSnapshot::initial(valid_config().validate().unwrap());
        let next_config = current
            .config
            .validate_replacement(valid_config())
            .unwrap();
        let next = current.replaced(next_config).unwrap();

        assert_eq!(current.generation, 1);
        assert_eq!(next.generation, 2);
    }
}

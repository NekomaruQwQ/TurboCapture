//! Complete instance configuration, validation, and generation snapshots.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::selector::{SelectorPolicy, validate_selection};

/// Maximum number of simultaneous key colors supported by the browser shader.
pub const MAX_RENDER_KEYS: usize = 8;

/// Four pixel-frames per output bit keeps local low-latency CBR visually conservative.
const INFERRED_BIT_RATE_PIXEL_FRAME_DIVISOR: u64 = 4;

/// One complete live capture-instance configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfig {
    /// Policy controlling which observed window owns the stream.
    pub selection: SelectionConfig,
    /// Source capture and processing that may change without recreating the encoder.
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
    /// keys, enabled names, and matching rules are trimmed before storage.
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
    /// Profile names enabled in descending priority order.
    #[serde(default)]
    pub enabled: Vec<String>,
    /// Reusable profile definitions keyed by stable human-facing name.
    #[serde(default)]
    pub profiles: BTreeMap<String, SelectionProfileConfig>,
}

/// One selector profile definition and its executable-path rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionProfileConfig {
    /// Case-insensitive executable-path substrings accepted by this profile.
    #[serde(default)]
    pub include: Vec<String>,
    /// Global case-insensitive vetoes contributed by this profile.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Source capture and processing that is independent of encoded output geometry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// Whether Windows Graphics Capture includes the mouse cursor.
    #[serde(default)]
    pub capture_cursor: bool,
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
    /// Optional H.264 bitrate override in bits per second.
    ///
    /// When absent, [`Self::target_bit_rate`] derives a high-fidelity local-stream
    /// target from output geometry and cadence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u32>,
}

impl VideoConfig {
    /// Returns the configured bitrate or a high-fidelity target inferred at 0.25
    /// bits per pixel per frame.
    ///
    /// Extreme unvalidated inputs saturate at Media Foundation's `u32` attribute
    /// limit rather than overflowing. Validated practical video modes remain exact.
    pub fn target_bit_rate(&self) -> u32 {
        self.bit_rate.unwrap_or_else(|| {
            let pixel_frames_per_second = u64::from(self.width)
                .saturating_mul(u64::from(self.height))
                .saturating_mul(u64::from(self.frame_rate));
            let inferred = pixel_frames_per_second / INFERRED_BIT_RATE_PIXEL_FRAME_DIVISOR;
            u32::try_from(inferred).unwrap_or(u32::MAX)
        })
    }
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

/// Loads and validates a UTF-8 TOML configuration document.
///
/// # Errors
///
/// Returns [`ConfigError`] for filesystem, UTF-8, TOML, or semantic validation
/// failures. No usable partial configuration is returned.
pub fn load_config(path: &Path) -> Result<ValidatedInstanceConfig, ConfigError> {
    let bytes = fs::read(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })?;
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
    if video.bit_rate == Some(0) {
        issues.push(ValidationIssue::new(
            "video.bit_rate",
            "zero_bit_rate",
            "bit_rate override must be non-zero"));
    }
}

/// Validate renderer limits and ensure overrides name defined selector profiles.
fn validate_render(
    selection: &SelectionConfig,
    render: &RenderProfiles,
    issues: &mut Vec<ValidationIssue>) {
    validate_render_config("render.default", &render.default, issues);

    for (profile_name, config) in &render.profiles {
        let path = format!("render.profiles.{profile_name}");
        if !selection.profiles.contains_key(profile_name) {
            issues.push(ValidationIssue::new(
                &path,
                "unknown_render_profile",
                "render override must name a defined selection profile"));
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
                enabled: vec!["code".to_owned()],
                profiles: BTreeMap::from([(
                    "code".to_owned(),
                    SelectionProfileConfig {
                        include: vec!["Code.exe".to_owned()],
                        exclude: vec![],
                    })]),
            },
            source: SourceConfig::default(),
            video: VideoConfig {
                width: 1920,
                height: 1200,
                frame_rate: 60,
                bit_rate: None,
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
    fn video_bit_rate_should_scale_with_pixel_rate() {
        let actual = [
            (1280, 720, 60),
            (1920, 1080, 30),
            (1920, 1080, 60),
            (2560, 1440, 60),
            (3840, 2160, 60),
        ].map(|(width, height, frame_rate)| VideoConfig {
            width,
            height,
            frame_rate,
            bit_rate: None,
        }.target_bit_rate());

        assert_eq!(actual, [13_824_000, 15_552_000, 31_104_000, 55_296_000, 124_416_000]);
    }

    #[test]
    fn explicit_video_bit_rate_should_override_inference() {
        let mut video = valid_config().video;
        video.bit_rate = Some(18_000_000);

        assert_eq!(video.target_bit_rate(), 18_000_000);
    }

    #[test]
    fn inferred_video_bit_rate_should_saturate_for_unvalidated_extreme_inputs() {
        let video = VideoConfig {
            width: u32::MAX,
            height: u32::MAX,
            frame_rate: u32::MAX,
            bit_rate: None,
        };

        assert_eq!(video.target_bit_rate(), u32::MAX);
    }

    #[test]
    fn configuration_should_allow_omitted_video_bit_rate() {
        let config = toml::from_str::<InstanceConfig>("
            [selection]

            [video]
            width = 1920
            height = 1080
            frame_rate = 60
        ").unwrap().validate().unwrap();

        assert_eq!(
            (config.config().video.bit_rate, config.config().video.target_bit_rate()),
            (None, 31_104_000));
    }

    #[test]
    fn configuration_should_disable_cursor_capture_by_default() {
        let config = toml::from_str::<InstanceConfig>("
            [selection]

            [video]
            width = 1920
            height = 1080
            frame_rate = 60
        ").unwrap().validate().unwrap();

        assert!(!config.config().source.capture_cursor);
    }

    #[test]
    fn configuration_should_accept_enabled_cursor_capture() {
        let config = toml::from_str::<InstanceConfig>("
            [selection]

            [source]
            capture_cursor = true

            [video]
            width = 1920
            height = 1080
            frame_rate = 60
        ").unwrap().validate().unwrap();

        assert!(config.config().source.capture_cursor);
    }

    #[test]
    fn validation_should_reject_zero_video_bit_rate_override() {
        let mut config = valid_config();
        config.video.bit_rate = Some(0);

        let ConfigError::Invalid { issues } = config.validate().unwrap_err() else {
            panic!("expected semantic validation failure");
        };
        assert_eq!(issues[0].code, "zero_bit_rate");
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
    fn render_override_should_allow_a_disabled_defined_profile() {
        let mut config = valid_config();
        config.selection.enabled.clear();
        config.render.profiles.insert("code".to_owned(), RenderConfig::default());

        config.validate().unwrap();
    }

    #[test]
    fn selection_profile_names_should_be_canonicalized_across_catalog_and_enabled_list() {
        let mut config = valid_config();
        let profile = config.selection.profiles.remove("code").unwrap();
        config.selection.profiles.insert("  code  ".to_owned(), profile);
        config.selection.enabled[0] = "  code  ".to_owned();

        let validated = config.validate().unwrap();

        assert!(validated.config().selection.profiles.contains_key("code"));
        assert_eq!(validated.config().selection.enabled, ["code"]);
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

//! Platform-independent policy, protocol, and service implementation for one
//! TurboCapture stream instance.
//!
//! A platform host supplies already-observed window facts, media status, and
//! encoded H.264 access units through bounded channels. This crate owns no
//! Windows handles, graphics resources, encoders, or process lifecycle.

#![deny(missing_docs)]

pub mod args;
pub mod config;
pub mod selector;
pub mod service;
pub mod status;
pub mod video;

pub use args::{AdapterLuid, CaptureArgs, ParseAdapterLuidError};
pub use config::{
    ColorKeyKnee, ConfigError, ConfigSnapshot, CropRect, InstanceConfig,
    RenderConfig, RenderProfiles, RgbColor, SelectionConfig,
    SelectionProfileConfig, SourceConfig, ValidatedInstanceConfig,
    ValidationIssue, VideoConfig, load_config,
};
pub use selector::{
    ObservationId, ObservedWindow, Selection, SelectionDecision, SelectorPolicy,
    WindowBounds, select_window,
};
pub use service::{
    ChannelCapacities, HostChannels, InstanceService, MediaCommand,
    MediaCompletion, ServiceError,
};
pub use status::{CaptureState, MediaStatus, RecoverableDiagnostic, TargetSummary};
pub use video::{
    AccessUnit, CodecConfiguration, VideoEvent, VideoMessage, VideoProtocolError,
    decode_message, encode_event, serialize_avcc,
};

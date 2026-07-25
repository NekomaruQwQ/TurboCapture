//! Cross-process D3D11 texture mailbox shared by capture and encoding workers.
//!
//! The supervisor owns the NT shared handle and underlying BGRA texture. Each
//! worker opens that handle on the explicitly selected adapter, then uses a
//! two-key `IDXGIKeyedMutex` protocol: key 0 grants producer access and key 1
//! grants consumer access. The mutex status is read from the raw COM vtable
//! because the generated Windows wrapper discards successful Win32 statuses
//! such as `WAIT_TIMEOUT` and `WAIT_ABANDONED`.

use std::{
    fmt,
    mem::size_of,
    str::FromStr,
};

use anyhow::Context as _;
use euclid::default::Size2D;
use windows::{
    core::{HRESULT, Interface as _, PCWSTR},
    Win32::{
        Foundation::{
            CloseHandle,
            HMODULE,
            HANDLE,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAGS,
            LUID,
            SetHandleInformation,
            WAIT_ABANDONED,
            WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET,
                D3D11_BIND_SHADER_RESOURCE,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_FLAG,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
                D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
                D3D11_SDK_VERSION,
                D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_DEFAULT,
                D3D11CreateDevice,
                ID3D11Device,
                ID3D11Device1,
                ID3D11DeviceContext,
                ID3D11Multithread,
                ID3D11Texture2D,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                CreateDXGIFactory1,
                DXGI_ERROR_ACCESS_LOST,
                DXGI_ERROR_DEVICE_HUNG,
                DXGI_ERROR_DEVICE_REMOVED,
                DXGI_ERROR_DEVICE_RESET,
                DXGI_ERROR_DRIVER_INTERNAL_ERROR,
                DXGI_ERROR_NOT_FOUND,
                DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
                DXGI_SHARED_RESOURCE_READ,
                DXGI_SHARED_RESOURCE_WRITE,
                IDXGIAdapter,
                IDXGIFactory6,
                IDXGIKeyedMutex,
                IDXGIResource1,
            },
        },
        Security::SECURITY_ATTRIBUTES,
    },
};

/// Mutex key granting the capture worker permission to publish a frame.
pub const PRODUCER_KEY: u64 = 0;
/// Mutex key granting the encoder worker permission to consume a frame.
pub const CONSUMER_KEY: u64 = 1;
/// Stable worker exit code requesting complete shared-resource recreation.
pub const RESOURCE_GENERATION_LOST_EXIT_CODE: i32 = 20;

/// Fatal condition proving the current shared-resource generation is unusable.
///
/// Workers wrap keyed-mutex abandonment and irrecoverable mailbox failures in
/// this type. The supervisor consumes only the stable process exit code; the
/// Rust type keeps in-process error classification independent from messages.
#[derive(Debug)]
pub struct ResourceGenerationLost {
    /// Human-readable cause retained for the worker's stderr diagnostics.
    reason: String,
}

impl ResourceGenerationLost {
    /// Classify one failure as requiring a new mailbox and GPU-worker cohort.
    pub fn new(reason: impl Into<String>) -> Self { Self { reason: reason.into() } }
}

impl fmt::Display for ResourceGenerationLost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ResourceGenerationLost {}

/// Decide whether a worker error invalidates the complete resource generation.
///
/// Explicit mailbox failures use [`ResourceGenerationLost`]. DXGI device-loss
/// HRESULTs are also recognized through an `anyhow` context chain so callers do
/// not need to discard useful API-specific context merely to classify failure.
pub fn is_resource_generation_lost(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<ResourceGenerationLost>().is_some() ||
            cause
                .downcast_ref::<windows::core::Error>()
                .is_some_and(|error| is_device_loss_hresult(error.code()))
    })
}

/// Identify the DXGI statuses that require adapter/resource reselection.
const fn is_device_loss_hresult(status: HRESULT) -> bool {
    status.0 == DXGI_ERROR_ACCESS_LOST.0 ||
        status.0 == DXGI_ERROR_DEVICE_HUNG.0 ||
        status.0 == DXGI_ERROR_DEVICE_REMOVED.0 ||
        status.0 == DXGI_ERROR_DEVICE_RESET.0 ||
        status.0 == DXGI_ERROR_DRIVER_INTERNAL_ERROR.0 ||
        status.0 == DXGI_ERROR_NOT_FOUND.0
}

/// Stable command-line representation of a DXGI adapter LUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AdapterLuid(u64);

impl AdapterLuid {
    /// Convert the two-part Win32 LUID without losing the signed high word.
    pub const fn from_raw(luid: LUID) -> Self {
        Self(((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64)
    }

    /// Reconstruct the Win32 LUID used by `EnumAdapterByLuid`.
    pub const fn as_raw(self) -> LUID {
        LUID {
            LowPart: self.0 as u32,
            HighPart: (self.0 >> 32) as u32 as i32,
        }
    }

    /// Return the opaque 64-bit value suitable for diagnostics and CLI transport.
    pub const fn value(self) -> u64 { self.0 }
}

impl fmt::Display for AdapterLuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:016X}", self.0)
    }
}

impl FromStr for AdapterLuid {
    type Err = String;

    /// Accept decimal or `0x`-prefixed hexadecimal values emitted by the supervisor.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_nonzero_u64(value, "adapter LUID").map(Self)
    }
}

/// Copyable command-line representation of an inherited NT handle.
///
/// Keeping parsing separate from ownership satisfies CLI parser requirements
/// without ever cloning an owning kernel-handle wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SharedHandleValue(usize);

impl SharedHandleValue {
    /// Transfer responsibility for closing the child's inherited handle.
    pub const fn into_owned(self) -> InheritedHandle {
        InheritedHandle(HANDLE(self.0 as *mut core::ffi::c_void))
    }

    /// Return the numeric value used in worker arguments.
    pub const fn value(self) -> usize { self.0 }
}

impl FromStr for SharedHandleValue {
    type Err = String;

    /// Accept decimal or `0x`-prefixed hexadecimal values from the parent CLI.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parsed = parse_nonzero_u64(value, "shared texture handle")?;
        let handle_value = usize::try_from(parsed)
            .map_err(|_overflow| format!("shared texture handle '{value}' does not fit this process"))?;
        Ok(Self(handle_value))
    }
}

/// Owning child-process wrapper for one inherited NT handle.
///
/// Opening the D3D11 resource creates an independent COM reference, so the
/// handle should be dropped immediately afterwards instead of leaking for the
/// worker lifetime.
#[derive(Debug)]
pub struct InheritedHandle(HANDLE);

impl InheritedHandle {
    /// Borrow the raw handle for `OpenSharedResource1`.
    pub const fn as_raw(&self) -> HANDLE { self.0 }

    /// Return the numeric value used only for diagnostics.
    pub fn value(&self) -> usize { self.0.0 as usize }
}

impl Drop for InheritedHandle {
    fn drop(&mut self) {
        // SAFETY: The value was inherited from the proof supervisor and this
        // wrapper is its sole owner in the child process.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Explicit D3D11 device cohort selected for one shared-resource generation.
pub struct DeviceBundle {
    /// Factory used for adapter lookup and selector preview swap-chain creation.
    pub factory: IDXGIFactory6,
    /// Device created on [`Self::adapter_luid`].
    pub device: ID3D11Device,
    /// Immediate context paired with [`Self::device`].
    pub context: ID3D11DeviceContext,
    /// Adapter identity passed unchanged to both GPU workers.
    pub adapter_luid: AdapterLuid,
    /// Human-readable adapter name for startup diagnostics.
    pub adapter_name: String,
}

/// Select the highest-performance adapter and create its D3D11 device.
pub fn create_high_performance_device(video_support: bool) -> anyhow::Result<DeviceBundle> {
    // SAFETY: DXGI factory creation has no caller preconditions. Factory1 is a
    // keyed-mutex invariant: merely querying IDXGIFactory6 from a factory-1.0
    // object does not make devices created from its adapters DXGI-1.1-derived.
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory6>() }
        .context("failed to create DXGI factory")?;
    // SAFETY: `factory` is valid and index zero requests the first matching adapter.
    let adapter = unsafe {
        factory.EnumAdapterByGpuPreference::<IDXGIAdapter>(
            0,
            DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
    }.context("failed to select high-performance DXGI adapter")?;
    create_device(factory, &adapter, video_support)
}

/// Create a D3D11 device on the supervisor-selected adapter.
pub fn create_device_on_adapter(
    adapter_luid: AdapterLuid,
    video_support: bool) -> anyhow::Result<DeviceBundle> {
    // SAFETY: DXGI factory creation has no caller preconditions. Factory1 is
    // required for `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX` resources.
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory6>() }
        .context("failed to create DXGI factory")?;
    // SAFETY: The LUID is an opaque value supplied by the supervisor. DXGI
    // validates whether it identifies an adapter in this process.
    let adapter = unsafe {
        factory.EnumAdapterByLuid::<IDXGIAdapter>(adapter_luid.as_raw())
    }.with_context(|| format!("failed to find DXGI adapter {adapter_luid}"))?;
    let bundle = create_device(factory, &adapter, video_support)?;
    anyhow::ensure!(
        bundle.adapter_luid == adapter_luid,
        "DXGI returned adapter {} for requested {adapter_luid}",
        bundle.adapter_luid);
    Ok(bundle)
}

/// Create and configure one multithread-protected device for `adapter`.
fn create_device(
    factory: IDXGIFactory6,
    adapter: &IDXGIAdapter,
    video_support: bool) -> anyhow::Result<DeviceBundle> {
    // SAFETY: `adapter` is a live DXGI interface and `GetDesc` fills a local value.
    let descriptor = unsafe { adapter.GetDesc() }.context("failed to describe DXGI adapter")?;
    let adapter_luid = AdapterLuid::from_raw(descriptor.AdapterLuid);
    let name_length = descriptor.Description
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(descriptor.Description.len());
    let adapter_name = String::from_utf16_lossy(&descriptor.Description[..name_length]);

    let mut device = None;
    let mut context = None;
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT |
        if video_support {
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT
        } else {
            D3D11_CREATE_DEVICE_FLAG(0)
        };
    // SAFETY: `adapter` remains alive during the call; output pointers refer to
    // initialized stack-local `Option`s and the requested feature level is valid.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            Some(&raw mut context))
    }.context("failed to create D3D11 device")?;
    let device = device.context("D3D11 returned a null device")?;
    let context = context.context("D3D11 returned a null immediate context")?;

    // The COM cast validates support and retains its own device reference.
    let multithread = device.cast::<ID3D11Multithread>()
        .context("failed to query D3D11 multithread protection")?;
    // SAFETY: `multithread` is a valid interface for this device.
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    Ok(DeviceBundle {
        factory,
        device,
        context,
        adapter_luid,
        adapter_name,
    })
}

/// Supervisor-owned shared BGRA mailbox and scoped-inheritance NT handle.
pub struct OwnedMailbox {
    /// Device cohort retaining the selected adapter and immediate context.
    device_bundle: DeviceBundle,
    /// Underlying shared texture retained for the resource-generation lifetime.
    _texture: ID3D11Texture2D,
    /// Keyed mutex interface for supervisor-side diagnostics.
    _mutex: SharedKeyedMutex,
    /// Fixed dimensions validated independently by each worker.
    _size: Size2D<u32>,
    /// Resource handle made inheritable only around intended worker spawns.
    handle: OwnedHandle,
}

impl OwnedMailbox {
    /// Create a two-key BGRA mailbox on the highest-performance adapter.
    pub fn new(size: Size2D<u32>) -> anyhow::Result<Self> {
        validate_size(size)?;
        let device_bundle = create_high_performance_device(false)?;
        let descriptor = shared_texture_descriptor(size);
        let mut texture = None;
        // SAFETY: The descriptor is a valid default-usage, non-multisampled BGRA
        // texture. The stack-local output starts as `None`.
        unsafe {
            device_bundle.device.CreateTexture2D(
                &raw const descriptor,
                None,
                Some(&raw mut texture))
        }.context("failed to create shared BGRA texture")?;
        let texture = texture.context("D3D11 returned a null shared texture")?;
        // A texture created with `SHARED_NTHANDLE` exposes IDXGIResource1.
        let resource = texture.cast::<IDXGIResource1>()
            .context("shared texture does not expose IDXGIResource1")?;
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: core::ptr::null_mut(),
            // The supervisor enables inheritance through an RAII guard only
            // while it synchronously launches an intended GPU worker.
            bInheritHandle: false.into(),
        };
        let access = DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0;
        // SAFETY: `security` remains alive during the call. A null name keeps
        // the resource private to explicitly inherited handle values.
        let handle = unsafe {
            resource.CreateSharedHandle(
                Some(&raw const security),
                access,
                PCWSTR::null())
        }.context("failed to create inheritable shared-texture handle")?;
        // `SHARED_KEYEDMUTEX` guarantees this query is supported.
        let keyed_mutex = texture.cast::<IDXGIKeyedMutex>()
            .context("shared texture does not expose IDXGIKeyedMutex")?;

        Ok(Self {
            device_bundle,
            _texture: texture,
            _mutex: SharedKeyedMutex(keyed_mutex),
            _size: size,
            handle: OwnedHandle(handle),
        })
    }

    /// Selected adapter and device retained by this resource generation.
    pub const fn device_bundle(&self) -> &DeviceBundle { &self.device_bundle }

    /// Temporarily permit one or more synchronous intended worker spawns.
    ///
    /// The mutable borrow prevents overlapping guards. Dropping the returned
    /// guard revokes inheritance on success, error, or unwind while preserving
    /// the parent handle and copies already inherited by children.
    pub fn inheritable_handle(&mut self) -> anyhow::Result<InheritableHandleGuard<'_>> {
        set_handle_inheritance(&self.handle, true)?;
        Ok(InheritableHandleGuard {
            handle: &mut self.handle,
            inheritable: true,
        })
    }
}

/// Scoped permission for directly spawned children to inherit the mailbox.
pub struct InheritableHandleGuard<'a> {
    /// Mutably borrowed owner prevents another simultaneous inheritance scope.
    handle: &'a mut OwnedHandle,
    /// Whether `Drop` still needs to perform best-effort revocation.
    inheritable: bool,
}

impl InheritableHandleGuard<'_> {
    /// Numeric handle value passed unchanged through the child command line.
    pub fn value(&self) -> usize { self.handle.0.0 as usize }

    /// Revoke inheritance explicitly before launching any unrelated child.
    ///
    /// Consuming the guard prevents further intended spawns in this scope. A
    /// failure remains visible to the supervisor instead of relying only on the
    /// best-effort `Drop` path.
    pub fn revoke(mut self) -> anyhow::Result<()> {
        set_handle_inheritance(self.handle, false)?;
        self.inheritable = false;
        Ok(())
    }
}

impl Drop for InheritableHandleGuard<'_> {
    fn drop(&mut self) {
        if !self.inheritable {
            return;
        }
        // This is best-effort in `Drop`; the successful enabling call proves the
        // handle is valid, and the owning handle remains private if no spawn ran.
        if let Err(error) = set_handle_inheritance(self.handle, false) {
            log::error!("failed to revoke shared-handle inheritance: {error:#}");
        }
    }
}

/// Toggle only the Win32 inheritance flag without changing ownership or access.
fn set_handle_inheritance(handle: &OwnedHandle, inheritable: bool) -> anyhow::Result<()> {
    let flags = if inheritable { HANDLE_FLAG_INHERIT } else { HANDLE_FLAGS::default() };
    // SAFETY: `handle` remains live and the mask changes only its inheritance
    // flag; no resource access rights or ownership are transferred here.
    unsafe { SetHandleInformation(handle.0, HANDLE_FLAG_INHERIT.0, flags) }
        .context("failed to update shared-handle inheritance")
}

/// Shared texture opened by one worker on the supervisor-selected adapter.
pub struct OpenedMailbox {
    /// Texture used as the producer target or consumer copy source.
    pub texture: ID3D11Texture2D,
    /// Two-key synchronization interface for the same texture.
    pub mutex: SharedKeyedMutex,
    /// Descriptor dimensions validated during opening.
    pub size: Size2D<u32>,
}

impl OpenedMailbox {
    /// Open and validate an inherited NT handle on `device`.
    ///
    /// The inherited handle remains owned by `handle` and closes when that CLI
    /// value is dropped. The opened texture and mutex retain independent COM
    /// references. Adapter, format, dimension, mip, and sample mismatches fail
    /// startup instead of invoking implicit copies or conversions.
    pub fn open(
        device: &ID3D11Device,
        handle: &InheritedHandle,
        expected_size: Size2D<u32>) -> anyhow::Result<Self> {
        validate_size(expected_size)?;
        // The cast validates D3D11.1 support and retains a device reference.
        let device1 = device.cast::<ID3D11Device1>()
            .context("D3D11 device does not expose ID3D11Device1")?;
        // SAFETY: `handle` is a live inherited NT resource handle. D3D validates
        // both access rights and adapter compatibility.
        let texture = unsafe {
            device1.OpenSharedResource1::<ID3D11Texture2D>(handle.as_raw())
        }.context("failed to open inherited shared texture")?;
        let mut descriptor = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `texture` is live and `GetDesc` writes a stack-local value.
        unsafe { texture.GetDesc(&raw mut descriptor); }
        validate_shared_texture_descriptor(&descriptor, expected_size)?;
        // Descriptor validation confirms `SHARED_KEYEDMUTEX` was set.
        let mutex = texture.cast::<IDXGIKeyedMutex>()
            .context("opened texture does not expose IDXGIKeyedMutex")?;

        Ok(Self {
            texture,
            mutex: SharedKeyedMutex(mutex),
            size: expected_size,
        })
    }
}

/// Outcome of one keyed-mutex acquisition attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AcquireStatus {
    /// The caller owns the shared texture until it releases the next key.
    Acquired,
    /// The requested key was unavailable before the caller's timeout.
    Timeout,
    /// The prior owner exited without releasing, invalidating this generation.
    Abandoned,
}

/// Raw-status-preserving wrapper around `IDXGIKeyedMutex`.
pub struct SharedKeyedMutex(IDXGIKeyedMutex);

impl SharedKeyedMutex {
    /// Attempt to acquire `key`, preserving timeout and abandonment statuses.
    pub fn acquire(&self, key: u64, timeout_ms: u32) -> anyhow::Result<AcquireStatus> {
        // SAFETY: `self.0` is live. Calling the raw vtable is necessary because
        // the generated wrapper converts every non-negative status into `Ok`.
        let status = unsafe {
            (IDXGIKeyedMutex::vtable(&self.0).AcquireSync)(
                self.0.as_raw(),
                key,
                timeout_ms)
        };
        match status.0 as u32 {
            value if value == WAIT_OBJECT_0.0 => Ok(AcquireStatus::Acquired),
            value if value == WAIT_TIMEOUT.0 => Ok(AcquireStatus::Timeout),
            value if value == WAIT_ABANDONED.0 => Ok(AcquireStatus::Abandoned),
            _ => {
                status.ok().context("keyed-mutex acquisition failed")?;
                anyhow::bail!("keyed-mutex acquisition returned unexpected status {status:?}")
            }
        }
    }

    /// Release ownership and grant the peer access through `next_key`.
    pub fn release(&self, next_key: u64) -> anyhow::Result<()> {
        // SAFETY: The caller must have received `AcquireStatus::Acquired` from
        // this mutex and must submit all shared-texture work before releasing.
        unsafe { self.0.ReleaseSync(next_key) }
            .context("failed to release keyed mutex")
    }
}

/// Owning supervisor-side kernel handle wrapper.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `CreateSharedHandle` returned this owned handle and this
        // wrapper is the sole supervisor-side owner.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Build the one allowed cross-process mailbox descriptor.
const fn shared_texture_descriptor(size: Size2D<u32>) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: size.width,
        Height: size.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32 | D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32 |
            D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
    }
}

/// Validate the shared-resource descriptor independently in each child.
fn validate_shared_texture_descriptor(
    descriptor: &D3D11_TEXTURE2D_DESC,
    expected_size: Size2D<u32>) -> anyhow::Result<()> {
    anyhow::ensure!(
        descriptor.Format == DXGI_FORMAT_B8G8R8A8_UNORM,
        "shared texture must use B8G8R8A8_UNORM (got {:?})",
        descriptor.Format);
    anyhow::ensure!(
        descriptor.Width == expected_size.width && descriptor.Height == expected_size.height,
        "shared texture dimensions must be {}x{} (got {}x{})",
        expected_size.width,
        expected_size.height,
        descriptor.Width,
        descriptor.Height);
    anyhow::ensure!(
        descriptor.MipLevels == 1 && descriptor.ArraySize == 1,
        "shared texture must be a single non-array mip");
    anyhow::ensure!(
        descriptor.SampleDesc.Count == 1 && descriptor.SampleDesc.Quality == 0,
        "shared texture must not be multisampled");
    let required_misc = D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32 |
        D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32;
    anyhow::ensure!(
        descriptor.MiscFlags & required_misc == required_misc,
        "shared texture is missing NT-handle or keyed-mutex flags");
    Ok(())
}

/// Reject empty textures before any D3D API invocation.
fn validate_size(size: Size2D<u32>) -> anyhow::Result<()> {
    anyhow::ensure!(size.width > 0 && size.height > 0, "shared texture dimensions must be non-zero");
    Ok(())
}

/// Parse a non-zero decimal or `0x`-prefixed command-line integer.
fn parse_nonzero_u64(value: &str, label: &str) -> Result<u64, String> {
    let parsed = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or_else(
            || value.parse::<u64>(),
            |hex| u64::from_str_radix(hex, 16))
        .map_err(|error| format!("invalid {label} '{value}': {error}"))?;
    if parsed == 0 {
        Err(format!("{label} must be non-zero"))
    } else {
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Provide the exact descriptor accepted by the mailbox contract.
    fn valid_descriptor() -> D3D11_TEXTURE2D_DESC {
        shared_texture_descriptor(Size2D::new(1920, 1200))
    }

    #[test]
    fn adapter_luid_round_trips_signed_high_word() {
        let raw = LUID { LowPart: 0x89AB_CDEF, HighPart: -2 };
        assert_eq!(AdapterLuid::from_raw(raw).as_raw(), raw);
    }

    #[test]
    fn cli_values_accept_decimal_and_hex() {
        assert_eq!("4660".parse::<AdapterLuid>().unwrap().value(), 0x1234);
        assert_eq!("0x1234".parse::<AdapterLuid>().unwrap().value(), 0x1234);
        assert_eq!("4660".parse::<SharedHandleValue>().unwrap().value(), 0x1234);
        assert_eq!("0X1234".parse::<SharedHandleValue>().unwrap().value(), 0x1234);
        "0".parse::<AdapterLuid>().unwrap_err();
        "0".parse::<SharedHandleValue>().unwrap_err();
    }

    #[test]
    fn shared_descriptor_requires_both_resource_flags() {
        let mut descriptor = valid_descriptor();
        descriptor.MiscFlags &= !(D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32);
        let error = validate_shared_texture_descriptor(
            &descriptor,
            Size2D::new(1920, 1200))
            .unwrap_err();
        assert!(error.to_string().contains("keyed-mutex"));
    }

    #[test]
    fn shared_descriptor_rejects_implicit_size_or_format_conversion() {
        let wrong_size = D3D11_TEXTURE2D_DESC { Width: 1280, ..valid_descriptor() };
        assert!(validate_shared_texture_descriptor(
            &wrong_size,
            Size2D::new(1920, 1200))
            .unwrap_err()
            .to_string()
            .contains("1280x1200"));

        let wrong_format = D3D11_TEXTURE2D_DESC {
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12,
            ..valid_descriptor()
        };
        assert!(validate_shared_texture_descriptor(
            &wrong_format,
            Size2D::new(1920, 1200))
            .unwrap_err()
            .to_string()
            .contains("B8G8R8A8_UNORM"));
    }

    #[test]
    fn device_loss_classification_is_explicit() {
        assert!(is_device_loss_hresult(DXGI_ERROR_DEVICE_REMOVED));
        assert!(is_device_loss_hresult(DXGI_ERROR_DEVICE_RESET));
        assert!(!is_device_loss_hresult(windows::Win32::Foundation::E_INVALIDARG));

        let explicit = anyhow::Error::new(ResourceGenerationLost::new("abandoned"));
        assert!(is_resource_generation_lost(&explicit));
        let ordinary = anyhow::anyhow!("ordinary worker failure");
        assert!(!is_resource_generation_lost(&ordinary));
    }
}

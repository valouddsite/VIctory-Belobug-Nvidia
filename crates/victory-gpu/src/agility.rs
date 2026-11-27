//! Load an app-supplied DirectX 12 Agility SDK on Windows 10.
//!
//! Windows 10's inbox `System32\D3D12Core.dll` stops at Shader Model 6.5 even
//! when the card and driver do SM 6.6+, and it has no mesh shader stage. This
//! fork's world is built from mesh pipelines (`Host::mesh_shaders` asks the
//! device for `D3D12_OPTIONS7` and SM 6.6), so on Windows 10 the inbox DLL
//! leaves those pipelines uncreatable and the world draws nothing.
//!
//! Windows 10 22H2 (build 19045.3393+, KB5029331) and Windows 11 expose
//! `ID3D12SDKConfiguration1::CreateDeviceFactory`, which loads a `D3D12Core.dll`
//! the app ships, without Developer Mode and without `D3D12SDKVersion` /
//! `D3D12SDKPath` PE exports. `ApplyToGlobalState` then points the process-wide
//! D3D12 entry points — device creation and `D3D12SerializeVersionedRootSignature`
//! among them — at that SDK instead of the inbox one.
//!
//! Search order (first folder holding a `D3D12Core.dll`):
//! 1. `%VICTORY_D3D12_DIR%` (when set and non-empty),
//! 2. `<exe dir>\D3D12`,
//! 3. `<exe dir>`.
//!
//! `%VICTORY_D3D12_SDK_VERSION%` overrides the version handed to
//! `CreateDeviceFactory`; 620 is the version this tree is tested against. No
//! SDK found, or an OS without `ID3D12SDKConfiguration1`: the title runs on
//! inbox D3D12 exactly as before.

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows::core::PCSTR;
use windows::Win32::Graphics::Direct3D12::{
    CLSID_D3D12SDKConfiguration, D3D12GetInterface, D3D12_DEVICE_FACTORY_FLAG_ALLOW_RETURNING_EXISTING_DEVICE,
    ID3D12DeviceFactory, ID3D12SDKConfiguration1,
};

/// Environment variable overriding the Agility SDK folder.
pub const ENV: &str = "VICTORY_D3D12_DIR";
const VERSION_ENV: &str = "VICTORY_D3D12_SDK_VERSION";
const DEFAULT_VERSION: u32 = 620;

#[derive(Debug)]
pub enum State {
    /// Loaded through `ID3D12SDKConfiguration1::CreateDeviceFactory`.
    Loaded {
        factory: ID3D12DeviceFactory,
        version: u32,
        dir: PathBuf,
    },
    /// No `D3D12Core.dll` in any search folder; inbox D3D12 is used.
    NotFound,
    /// A folder was found but the SDK could not be activated; inbox D3D12 is used.
    Unavailable(String),
}

static STATE: OnceLock<State> = OnceLock::new();

fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(v) = std::env::var_os(ENV) {
        let s = v.to_string_lossy().into_owned();
        let s = s.trim();
        if !s.is_empty() {
            // An explicit folder is honoured alone: silently falling through to
            // unrelated folders would hide a wrong setting.
            out.push(PathBuf::from(s));
            return out;
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(dir.join("D3D12"));
        out.push(dir.to_path_buf());
    }
    out
}

fn init() -> State {
    let Some(dir) = candidates().into_iter().find(|d| d.join("D3D12Core.dll").is_file()) else {
        return State::NotFound;
    };
    let version = std::env::var(VERSION_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_VERSION);
    // `CreateDeviceFactory` wants a NUL-terminated ANSI path with a trailing
    // backslash. A path that cannot be expressed that way fails here rather
    // than silently pointing at the wrong SDK.
    let mut path = dir.to_string_lossy().into_owned().replace('/', "\\");
    if !path.ends_with('\\') {
        path.push('\\');
    }
    let Ok(path) = CString::new(path) else {
        return State::Unavailable("the Agility SDK path holds a NUL".into());
    };
    // SAFETY: `D3D12GetInterface` writes an owned COM pointer into `cfg` on
    // success; `CreateDeviceFactory` takes a valid NUL-terminated path with a
    // trailing backslash and returns an owned `ID3D12DeviceFactory`.
    unsafe {
        let mut cfg: Option<ID3D12SDKConfiguration1> = None;
        if let Err(e) = D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut cfg) {
            return State::Unavailable(format!(
                "ID3D12SDKConfiguration1 unavailable (Windows 10 22H2 KB5029331+ needed): {e}"
            ));
        }
        let Some(cfg) = cfg else {
            return State::Unavailable("D3D12GetInterface returned no ID3D12SDKConfiguration1".into());
        };
        let factory: ID3D12DeviceFactory = match cfg
            .CreateDeviceFactory::<PCSTR, ID3D12DeviceFactory>(version, PCSTR(path.as_ptr().cast()))
        {
            Ok(factory) => factory,
            Err(e) => {
                return State::Unavailable(format!(
                    "CreateDeviceFactory(v{version}, {}): {e}",
                    dir.display()
                ));
            }
        };
        if let Err(e) = factory.SetFlags(D3D12_DEVICE_FACTORY_FLAG_ALLOW_RETURNING_EXISTING_DEVICE) {
            return State::Unavailable(format!("ID3D12DeviceFactory::SetFlags: {e}"));
        }
        // Point the process-wide D3D12 state at this SDK before the first
        // device exists, so that free functions the factory does not expose
        // (D3D12SerializeVersionedRootSignature in particular) also dispatch
        // to the Agility D3D12Core.dll.
        if let Err(e) = factory.ApplyToGlobalState() {
            return State::Unavailable(format!("ID3D12DeviceFactory::ApplyToGlobalState: {e}"));
        }
        State::Loaded {
            factory,
            version,
            dir,
        }
    }
}

/// Process-wide Agility SDK state (initialised on first use).
pub fn state() -> &'static State {
    STATE.get_or_init(init)
}

/// Log what the SDK search settled on; call once before the first device.
pub fn prepare() {
    match state() {
        State::Loaded { version, dir, .. } => {
            tracing::info!("D3D12: Agility SDK v{version} ({})", dir.display());
        }
        State::NotFound => tracing::info!(
            "D3D12: no Agility SDK folder; inbox D3D12 (Shader Model 6.5 on Windows 10, no mesh shaders)"
        ),
        State::Unavailable(reason) => {
            tracing::warn!("D3D12: Agility SDK unavailable ({reason}); inbox D3D12");
        }
    }
}

// The SDK is installed through the process-wide state (`ApplyToGlobalState`
// above), so callers keep using plain `D3D12CreateDevice`: after a successful
// `prepare()` that entry point dispatches into the Agility `D3D12Core.dll`.
// Nothing is redirected when the SDK is missing, which is the behaviour this
// tree has always had on Windows 11 and on machines without the folder.
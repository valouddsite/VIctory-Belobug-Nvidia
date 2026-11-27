//! DLSS integration preflight and safe fallback.
//!
//! The current presentation path copies/composites the guest image into the
//! host swap chain. It does not yet expose a complete, verified DLSS input
//! set (especially per-frame motion vectors and jitter), and Streamline is
//! not linked. This module deliberately never reports DLSS as active.
//!
//! Keep the input contract and output-resolution planning here so a later
//! Streamline backend can be added without silently enabling an incomplete
//! temporal upscaler.

use std::sync::OnceLock;

/// User-selectable DLSS quality preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    #[default]
    Quality,
    Balanced,
    Performance,
    UltraPerformance,
}

impl Quality {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "quality" => Some(Self::Quality),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            "ultra-performance" | "ultra_performance" => Some(Self::UltraPerformance),
            _ => None,
        }
    }

    /// Approximate internal-resolution scale used for planning only.
    /// The render path must not apply this until Streamline is operational.
    pub const fn scale(self) -> f32 {
        match self {
            Self::Quality => 0.6667,
            Self::Balanced => 0.58,
            Self::Performance => 0.5,
            Self::UltraPerformance => 0.3333,
        }
    }
}

/// Inputs that must be supplied and correctly tagged for temporal DLSS SR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameInputs {
    pub color: bool,
    pub depth: bool,
    pub motion_vectors: bool,
    pub jitter: bool,
    pub exposure: bool,
}

impl FrameInputs {
    /// Returns the required inputs that are absent. Exposure is optional for
    /// SDR; the other four are required for a valid temporal evaluation.
    pub fn missing_required(self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.color {
            missing.push("scene color");
        }
        if !self.depth {
            missing.push("depth");
        }
        if !self.motion_vectors {
            missing.push("motion vectors");
        }
        if !self.jitter {
            missing.push("sub-pixel jitter");
        }
        missing
    }

    pub fn ready(self) -> bool {
        self.missing_required().is_empty()
    }
}

/// Resolution plan for a future DLSS pass. This does not resize resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionPlan {
    pub render_width: u32,
    pub render_height: u32,
    pub output_width: u32,
    pub output_height: u32,
}

impl ResolutionPlan {
    pub fn new(output_width: u32, output_height: u32, quality: Quality) -> Option<Self> {
        if output_width == 0 || output_height == 0 {
            return None;
        }
        let scale = quality.scale();
        let scaled = |value: u32| ((value as f32 * scale).round() as u32).max(1);
        Some(Self {
            render_width: scaled(output_width),
            render_height: scaled(output_height),
            output_width,
            output_height,
        })
    }
}


/// Minimal resource metadata used to reject incompatible color/depth pairs
/// before a future temporal upscaler can consume them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureInfo {
    pub width: u32,
    pub height: u32,
    pub samples: u32,
    pub dimension: u32,
}

/// Why a color/depth pair cannot be used for temporal reconstruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthPairStatus {
    Compatible,
    MissingExtent,
    NotTexture2D,
    ExtentMismatch,
    SampleCountMismatch,
    UnsupportedDepthFormat,
}

/// Validate a scene-color texture against its bound depth view.
/// D3D12 texture2D dimension and DSV texture2D dimension are both 3.
pub fn validate_depth_pair(
    color: TextureInfo,
    depth: TextureInfo,
    depth_view_format: u32,
    depth_view_dimension: u32,
) -> DepthPairStatus {
    const TEXTURE2D: u32 = 3;
    const DEPTH_FORMATS: [u32; 4] = [20, 40, 45, 55];
    if color.width == 0 || color.height == 0 || depth.width == 0 || depth.height == 0
        || color.samples == 0 || depth.samples == 0
    {
        return DepthPairStatus::MissingExtent;
    }
    if color.dimension != TEXTURE2D || depth.dimension != TEXTURE2D || depth_view_dimension != TEXTURE2D {
        return DepthPairStatus::NotTexture2D;
    }
    if color.width != depth.width || color.height != depth.height {
        return DepthPairStatus::ExtentMismatch;
    }
    if color.samples != depth.samples {
        return DepthPairStatus::SampleCountMismatch;
    }
    if !DEPTH_FORMATS.contains(&depth_view_format) {
        return DepthPairStatus::UnsupportedDepthFormat;
    }
    DepthPairStatus::Compatible
}


/// Report each depth-pair validation outcome at most once per process.
/// This is diagnostic only; it does not authorize DLSS evaluation.
pub(crate) fn report_depth_pair_status(status: DepthPairStatus) {
    use std::sync::atomic::{AtomicU8, Ordering};
    static REPORTED: AtomicU8 = AtomicU8::new(0);
    let bit = 1u8 << (status as u8);
    let previous = REPORTED.fetch_or(bit, Ordering::Relaxed);
    if previous & bit != 0 {
        return;
    }
    match status {
        DepthPairStatus::Compatible => tracing::info!(
            "DLSS preflight: at least one bound color/depth target pair has matching 2D extent and sample count"
        ),
        other => tracing::warn!(?other, "DLSS preflight: bound color/depth target pair rejected"),
    }
}

/// A frame-local registry used to associate one presented color resource
/// with one unambiguous depth resource. Generic so its lifecycle can be
/// tested without constructing Direct3D resources.
struct FrameCandidateRegistry<T> {
    candidates: std::collections::HashMap<(u64, usize), Option<T>>,
}

impl<T> Default for FrameCandidateRegistry<T> {
    fn default() -> Self {
        Self {
            candidates: std::collections::HashMap::new(),
        }
    }
}

impl<T: PartialEq> FrameCandidateRegistry<T> {
    fn prune(&mut self, frame: u64) {
        self.candidates
            .retain(|(candidate_frame, _), _| *candidate_frame >= frame);
    }

    fn register(&mut self, frame: u64, color_resource: usize, candidate: T) {
        if color_resource == 0 {
            return;
        }
        self.prune(frame);
        match self.candidates.entry((frame, color_resource)) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Some(candidate));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().as_ref() != Some(&candidate) {
                    entry.insert(None);
                }
            }
        }
    }

    fn invalidate(&mut self, frame: u64, color_resource: usize) {
        if color_resource == 0 {
            return;
        }
        self.prune(frame);
        self.candidates.insert((frame, color_resource), None);
    }

    fn consume(&mut self, frame: u64, color_resource: usize) -> Option<T> {
        self.prune(frame);
        self.candidates.remove(&(frame, color_resource)).flatten()
    }
}

#[cfg(windows)]
pub(crate) struct PresentedDepthCandidate {
    color: std::sync::Arc<crate::resource::HostTexture>,
    depth: std::sync::Arc<crate::resource::HostTexture>,
}

#[cfg(windows)]
impl PartialEq for PresentedDepthCandidate {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.color, &other.color)
            && std::sync::Arc::ptr_eq(&self.depth, &other.depth)
    }
}

#[cfg(windows)]
impl Eq for PresentedDepthCandidate {}

#[cfg(windows)]
static PRESENT_DEPTH_CANDIDATES: OnceLock<
    std::sync::Mutex<FrameCandidateRegistry<PresentedDepthCandidate>>,
> = OnceLock::new();

/// Retain an exact color/depth pair for the frame that is about to be presented.
/// A color target paired with multiple different depth resources is marked
/// ambiguous and will not be exposed as a temporal input.
#[cfg(windows)]
pub(crate) fn register_present_depth_candidate(
    frame: u64,
    color_resource: usize,
    color: std::sync::Arc<crate::resource::HostTexture>,
    depth: std::sync::Arc<crate::resource::HostTexture>,
) {
    let candidates = PRESENT_DEPTH_CANDIDATES
        .get_or_init(|| std::sync::Mutex::new(FrameCandidateRegistry::default()));
    candidates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(frame, color_resource, PresentedDepthCandidate { color, depth });
}

/// Invalidate a color candidate when a later target binding has no compatible
/// depth view. Once invalidated, another pair in the same frame stays ambiguous.
#[cfg(windows)]
pub(crate) fn invalidate_present_depth_candidate(frame: u64, color_resource: usize) {
    let candidates = PRESENT_DEPTH_CANDIDATES
        .get_or_init(|| std::sync::Mutex::new(FrameCandidateRegistry::default()));
    candidates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .invalidate(frame, color_resource);
}

/// Consume a candidate only when the exact guest color resource is presented
/// in the same frame. The caller holds the returned resources through Present.
#[cfg(windows)]
pub(crate) fn consume_present_depth_candidate(
    frame: u64,
    color_resource: usize,
) -> Option<PresentedDepthCandidate> {
    let candidates = PRESENT_DEPTH_CANDIDATES.get()?;
    let candidate = candidates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .consume(frame, color_resource)?;
    static REPORTED: OnceLock<()> = OnceLock::new();
    REPORTED.get_or_init(|| {
        tracing::info!(
            "DLSS preflight: exact presented color resource matched an unambiguous depth resource recorded in the same frame"
        );
    });
    Some(candidate)
}

#[cfg(test)]
mod candidate_registry_tests {
    use super::FrameCandidateRegistry;

    #[test]
    fn exact_color_depth_pair_is_consumed_only_in_its_frame() {
        let mut registry = FrameCandidateRegistry::default();
        registry.register(10, 0x100, 0x200_u64);

        assert_eq!(registry.consume(9, 0x100), None);
        assert_eq!(registry.consume(10, 0x101), None);
        assert_eq!(registry.consume(10, 0x100), Some(0x200));
        assert_eq!(registry.consume(10, 0x100), None);
    }

    #[test]
    fn conflicting_depth_pairs_are_rejected() {
        let mut registry = FrameCandidateRegistry::default();
        registry.register(20, 0x100, 0x200_u64);
        registry.register(20, 0x100, 0x201_u64);

        assert_eq!(registry.consume(20, 0x100), None);
    }

    #[test]
    fn repeated_identical_pair_remains_unambiguous() {
        let mut registry = FrameCandidateRegistry::default();
        registry.register(30, 0x100, 0x200_u64);
        registry.register(30, 0x100, 0x200_u64);

        assert_eq!(registry.consume(30, 0x100), Some(0x200));
    }

    #[test]
    fn invalidation_is_sticky_for_the_frame() {
        let mut registry = FrameCandidateRegistry::default();
        registry.register(40, 0x100, 0x200_u64);
        registry.invalidate(40, 0x100);
        registry.register(40, 0x100, 0x200_u64);

        assert_eq!(registry.consume(40, 0x100), None);
    }

    #[test]
    fn stale_frames_are_pruned_without_removing_future_frames() {
        let mut registry = FrameCandidateRegistry::default();
        registry.register(50, 0x100, 0x200_u64);
        registry.register(51, 0x100, 0x201_u64);

        assert_eq!(registry.consume(51, 0x100), Some(0x201));
        assert_eq!(registry.consume(50, 0x100), None);
    }
}

/// True only when no title submissions are waiting to start and every host
/// submission that has started has completed. This is a conservative safety
/// gate, not proof that a particular candidate was produced by a particular
/// command list.
pub(crate) const fn submissions_quiescent(queued: u64, started: u64, finished: u64) -> bool {
    queued == 0 && started == finished
}

/// Report once when a candidate is discarded because GPU work is still pending.
#[cfg(windows)]
pub(crate) fn report_present_candidate_not_quiescent() {
    static REPORTED: OnceLock<()> = OnceLock::new();
    REPORTED.get_or_init(|| {
        tracing::warn!(
            "DLSS preflight: discarded the presented depth candidate because host GPU submissions were still queued or in flight"
        );
    });
}

#[cfg(test)]
mod submission_quiescence_tests {
    use super::submissions_quiescent;

    #[test]
    fn quiescence_requires_no_queued_or_in_flight_submissions() {
        assert!(submissions_quiescent(0, 12, 12));
        assert!(!submissions_quiescent(1, 12, 12));
        assert!(!submissions_quiescent(0, 12, 11));
        assert!(!submissions_quiescent(2, 12, 10));
    }
}

/// Current backend state. DLSS stays unavailable until Streamline is linked,
/// initialized, and fed by the real renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    NotIntegrated,
}

pub const fn backend_state() -> BackendState {
    BackendState::NotIntegrated
}

/// Warn once if a user explicitly requests DLSS. The current frame path
/// remains unchanged and is presented normally.
pub fn report_requested_but_unavailable() {
    static REQUESTED_QUALITY: OnceLock<Option<Quality>> = OnceLock::new();
    let requested = *REQUESTED_QUALITY.get_or_init(|| {
        let enabled = std::env::var_os("VICTORY_DLSS")
            .is_some_and(|value| value != "0" && !value.is_empty());
        enabled.then(|| {
            std::env::var("VICTORY_DLSS_QUALITY")
                .ok()
                .and_then(|value| Quality::parse(&value))
                .unwrap_or_default()
        })
    });
    static REPORTED: OnceLock<()> = OnceLock::new();
    if let Some(quality) = requested {
        REPORTED.get_or_init(|| {
            tracing::warn!(
                ?quality,
                "DLSS was requested but is NOT active: Streamline is not integrated and the renderer does not yet provide verified depth, motion-vector and jitter inputs; using the normal presentation path"
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_names_are_case_insensitive_and_invalid_values_are_rejected() {
        assert_eq!(Quality::parse("QUALITY"), Some(Quality::Quality));
        assert_eq!(Quality::parse("balanced"), Some(Quality::Balanced));
        assert_eq!(Quality::parse("Performance"), Some(Quality::Performance));
        assert_eq!(Quality::parse("ultra-performance"), Some(Quality::UltraPerformance));
        assert_eq!(Quality::parse("fastest"), None);
    }

    #[test]
    fn temporal_inputs_require_color_depth_motion_vectors_and_jitter() {
        let empty = FrameInputs::default();
        assert_eq!(
            empty.missing_required(),
            vec!["scene color", "depth", "motion vectors", "sub-pixel jitter"]
        );
        assert!(!empty.ready());

        let complete = FrameInputs {
            color: true,
            depth: true,
            motion_vectors: true,
            jitter: true,
            exposure: false,
        };
        assert!(complete.ready(), "exposure is optional for SDR");
    }

    #[test]
    fn quality_resolution_plans_are_smaller_than_output() {
        let plan = ResolutionPlan::new(2560, 1440, Quality::Quality).unwrap();
        assert_eq!((plan.output_width, plan.output_height), (2560, 1440));
        assert!(plan.render_width < plan.output_width);
        assert!(plan.render_height < plan.output_height);

        let performance =
            ResolutionPlan::new(2560, 1440, Quality::Performance).unwrap();
        assert!(performance.render_width < plan.render_width);
        assert!(performance.render_height < plan.render_height);
    }

    #[test]
    fn zero_sized_output_is_rejected() {
        assert!(ResolutionPlan::new(0, 1440, Quality::Quality).is_none());
        assert!(ResolutionPlan::new(2560, 0, Quality::Quality).is_none());
    }

    #[test]
    fn backend_is_never_misreported_as_active() {
        assert_eq!(backend_state(), BackendState::NotIntegrated);
    }
    #[test]
    fn depth_pair_validation_rejects_mismatched_or_untyped_inputs() {
        let color = TextureInfo { width: 1920, height: 1080, samples: 1, dimension: 3 };
        let depth = color;
        assert_eq!(validate_depth_pair(color, depth, 40, 3), DepthPairStatus::Compatible);
        assert_eq!(validate_depth_pair(color, TextureInfo { width: 1280, ..depth }, 40, 3), DepthPairStatus::ExtentMismatch);
        assert_eq!(validate_depth_pair(color, TextureInfo { samples: 4, ..depth }, 40, 3), DepthPairStatus::SampleCountMismatch);
        assert_eq!(validate_depth_pair(color, depth, 39, 3), DepthPairStatus::UnsupportedDepthFormat);
        assert_eq!(validate_depth_pair(color, depth, 40, 4), DepthPairStatus::NotTexture2D);
        assert_eq!(validate_depth_pair(color, TextureInfo { width: 0, ..depth }, 40, 3), DepthPairStatus::MissingExtent);
    }

}

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

/// Conservative classification of the host-side D3D12 state tracker.
/// This is diagnostic metadata only: it is not proof of queue ordering and
/// never authorizes a DLSS evaluation or substitutes for a resource barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostResourceStateClass {
    Common,
    ShaderResourceRead,
    OtherReadOnly,
    RenderTarget,
    DepthWrite,
    UnorderedAccess,
    CopySource,
    CopyDestination,
    Unknown,
}

/// Classify raw D3D12 resource-state bits.
///
/// D3D12 permits combinations of read-only states. A state is considered a
/// shader-resource read only when PIXEL or NON_PIXEL_SHADER_RESOURCE is set
/// and every bit belongs to the conservative read-only mask. Write states and
/// unrecognized bits are never classified as shader-readable.
pub const fn classify_host_resource_state(state: u32) -> HostResourceStateClass {
    const VERTEX_AND_CONSTANT_BUFFER: u32 = 0x1;
    const INDEX_BUFFER: u32 = 0x2;
    const RENDER_TARGET: u32 = 0x4;
    const UNORDERED_ACCESS: u32 = 0x8;
    const DEPTH_WRITE: u32 = 0x10;
    const DEPTH_READ: u32 = 0x20;
    const NON_PIXEL_SHADER_RESOURCE: u32 = 0x40;
    const PIXEL_SHADER_RESOURCE: u32 = 0x80;
    const INDIRECT_ARGUMENT: u32 = 0x200;
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    const SHADER_READ: u32 = NON_PIXEL_SHADER_RESOURCE | PIXEL_SHADER_RESOURCE;
    const READ_ONLY: u32 = VERTEX_AND_CONSTANT_BUFFER
        | INDEX_BUFFER
        | DEPTH_READ
        | NON_PIXEL_SHADER_RESOURCE
        | PIXEL_SHADER_RESOURCE
        | INDIRECT_ARGUMENT
        | COPY_SOURCE;

    if state == 0 {
        HostResourceStateClass::Common
    } else if state == RENDER_TARGET {
        HostResourceStateClass::RenderTarget
    } else if state == DEPTH_WRITE {
        HostResourceStateClass::DepthWrite
    } else if state == UNORDERED_ACCESS {
        HostResourceStateClass::UnorderedAccess
    } else if state == COPY_DEST {
        HostResourceStateClass::CopyDestination
    } else if state == COPY_SOURCE {
        HostResourceStateClass::CopySource
    } else if state & SHADER_READ != 0 && state & !READ_ONLY == 0 {
        HostResourceStateClass::ShaderResourceRead
    } else if state & !READ_ONLY == 0 {
        HostResourceStateClass::OtherReadOnly
    } else {
        // Includes write/read mixtures and state bits not covered above.
        HostResourceStateClass::Unknown
    }
}

/// Whether a tracked state can be read through a D3D12 shader-resource view.
/// This is only a diagnostic predicate; it does not establish synchronization.
pub const fn is_shader_resource_readable(state: u32) -> bool {
    matches!(
        classify_host_resource_state(state),
        HostResourceStateClass::ShaderResourceRead
    )
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

trait SubmissionTracked {
    fn recording_list(&self) -> usize;
    fn mark_submitted(&mut self, submission: u64);
    fn submission_completed(&self, finished: u64) -> bool;
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

impl<T: PartialEq + SubmissionTracked> FrameCandidateRegistry<T> {
    fn mark_submitted(&mut self, recording_list: usize, submission: u64) {
        for candidate in self.candidates.values_mut().flatten() {
            if candidate.recording_list() == recording_list {
                candidate.mark_submitted(submission);
            }
        }
    }

    fn consume_completed(
        &mut self,
        frame: u64,
        color_resource: usize,
        finished_submission: u64,
    ) -> Option<T> {
        let candidate = self.consume(frame, color_resource)?;
        candidate.submission_completed(finished_submission).then_some(candidate)
    }
}

#[cfg(windows)]
pub(crate) struct PresentedDepthCandidate {
    color: std::sync::Arc<crate::resource::HostTexture>,
    depth: std::sync::Arc<crate::resource::HostTexture>,
    recording_list: usize,
    submission: Option<u64>,
}

#[cfg(windows)]
impl PartialEq for PresentedDepthCandidate {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.color, &other.color)
            && std::sync::Arc::ptr_eq(&self.depth, &other.depth)
            && self.recording_list == other.recording_list
    }
}

#[cfg(windows)]
impl Eq for PresentedDepthCandidate {}

#[cfg(windows)]
impl SubmissionTracked for PresentedDepthCandidate {
    fn recording_list(&self) -> usize {
        self.recording_list
    }

    fn mark_submitted(&mut self, submission: u64) {
        match self.submission {
            None => self.submission = Some(submission),
            Some(existing) if existing == submission => {}
            Some(_) => self.submission = None,
        }
    }

    fn submission_completed(&self, finished: u64) -> bool {
        self.submission.is_some_and(|submission| submission <= finished)
    }
}

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
    recording_list: usize,
) {
    let candidates = PRESENT_DEPTH_CANDIDATES
        .get_or_init(|| std::sync::Mutex::new(FrameCandidateRegistry::default()));
    candidates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(
            frame,
            color_resource,
            PresentedDepthCandidate { color, depth, recording_list, submission: None },
        );
}

/// Bind candidates recorded by this host list to the submission token assigned
/// by Host::submit. Candidates never included in a submission remain unusable.
#[cfg(windows)]
pub(crate) fn mark_present_depth_candidates_submitted(recording_list: usize, submission: u64) {
    let Some(candidates) = PRESENT_DEPTH_CANDIDATES.get() else {
        return;
    };
    candidates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mark_submitted(recording_list, submission);
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
    finished_submission: u64,
) -> Option<PresentedDepthCandidate> {
    let candidates = PRESENT_DEPTH_CANDIDATES.get()?;
    let candidate = candidates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .consume_completed(frame, color_resource, finished_submission)?;
    // Snapshot the existing host state tracker for diagnostics only. The
    // values are not a proof of GPU visibility and do not trigger barriers.
    use std::sync::atomic::Ordering;
    let color_state = candidate.color.state.load(Ordering::Acquire);
    let depth_state = candidate.depth.state.load(Ordering::Acquire);
    static STATE_REPORTED: OnceLock<()> = OnceLock::new();
    STATE_REPORTED.get_or_init(|| {
        tracing::info!(
            color_state,
            depth_state,
            color_class = ?classify_host_resource_state(color_state),
            depth_class = ?classify_host_resource_state(depth_state),
            color_shader_readable = is_shader_resource_readable(color_state),
            depth_shader_readable = is_shader_resource_readable(depth_state),
            "DLSS preflight: sampled tracked D3D12 states for the completed color/depth candidate; shader-read flags are diagnostic only, no barriers or evaluation"
        );
    });
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

    #[derive(PartialEq)]
    struct Tracked {
        list: usize,
        submission: Option<u64>,
    }

    impl super::SubmissionTracked for Tracked {
        fn recording_list(&self) -> usize { self.list }
        fn mark_submitted(&mut self, submission: u64) { self.submission = Some(submission); }
        fn submission_completed(&self, finished: u64) -> bool {
            self.submission.is_some_and(|value| value <= finished)
        }
    }

    #[test]
    fn candidate_requires_its_recording_list_to_be_submitted_and_finished() {
        let mut registry = FrameCandidateRegistry::default();
        registry.register(60, 0x100, Tracked { list: 0xabc, submission: None });
        registry.mark_submitted(0xdef, 7);
        assert!(registry.consume_completed(60, 0x100, 99).is_none());

        registry.register(61, 0x100, Tracked { list: 0xabc, submission: None });
        registry.mark_submitted(0xabc, 8);
        assert!(registry.consume_completed(61, 0x100, 7).is_none());

        registry.register(62, 0x100, Tracked { list: 0xabc, submission: None });
        registry.mark_submitted(0xabc, 9);
        assert_eq!(
            registry.consume_completed(62, 0x100, 9).map(|item| item.submission),
            Some(Some(9))
        );
    }

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
    fn host_resource_state_classification_is_conservative() {
        assert_eq!(classify_host_resource_state(0), HostResourceStateClass::Common);
        assert_eq!(classify_host_resource_state(0x4), HostResourceStateClass::RenderTarget);
        assert_eq!(classify_host_resource_state(0x10), HostResourceStateClass::DepthWrite);
        assert_eq!(classify_host_resource_state(0x8), HostResourceStateClass::UnorderedAccess);
        assert_eq!(classify_host_resource_state(0x800), HostResourceStateClass::CopySource);
        assert_eq!(classify_host_resource_state(0x400), HostResourceStateClass::CopyDestination);
        assert_eq!(classify_host_resource_state(0x40 | 0x80), HostResourceStateClass::ShaderResourceRead);
        assert_eq!(classify_host_resource_state(0x20 | 0x40), HostResourceStateClass::ShaderResourceRead);
        assert_eq!(classify_host_resource_state(0x1 | 0x2), HostResourceStateClass::OtherReadOnly);
        assert_eq!(classify_host_resource_state(0x200), HostResourceStateClass::OtherReadOnly);
        assert_eq!(classify_host_resource_state(0x4 | 0x80), HostResourceStateClass::Unknown);
        assert_eq!(classify_host_resource_state(0x100), HostResourceStateClass::Unknown);
        assert_eq!(classify_host_resource_state(0x1000), HostResourceStateClass::Unknown);
    }

    #[test]
    fn shader_resource_readability_requires_shader_read_bits_and_no_write_state() {
        assert!(is_shader_resource_readable(0x40));
        assert!(is_shader_resource_readable(0x80));
        assert!(is_shader_resource_readable(0x20 | 0x40 | 0x80));
        assert!(is_shader_resource_readable(0x800 | 0x40));
        assert!(!is_shader_resource_readable(0));
        assert!(!is_shader_resource_readable(0x20));
        assert!(!is_shader_resource_readable(0x4));
        assert!(!is_shader_resource_readable(0x10));
        assert!(!is_shader_resource_readable(0x8));
        assert!(!is_shader_resource_readable(0x4 | 0x80));
    }

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

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

/// Units used by the renderer when it writes motion-vector components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionVectorUnits {
    /// Motion vectors are expressed in render-resolution pixels.
    Pixels,
    /// Motion vectors are normalized to the render extent.
    Normalized,
}

/// Direction of the motion-vector displacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionVectorDirection {
    /// Vector points from the current pixel to its previous-frame location.
    CurrentToPrevious,
    /// Vector points from the previous-frame pixel to its current location.
    PreviousToCurrent,
}

/// Per-frame metadata needed before motion vectors and jitter can be tagged
/// for a future Streamline DLSS SR evaluation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TemporalFrameMetadata {
    pub render_width: u32,
    pub render_height: u32,
    pub motion_vector_width: u32,
    pub motion_vector_height: u32,
    pub motion_vector_scale_x: f32,
    pub motion_vector_scale_y: f32,
    /// Sub-pixel projection jitter in render-pixel units.
    pub jitter_x_pixels: f32,
    pub jitter_y_pixels: f32,
    pub motion_vector_units: MotionVectorUnits,
    pub motion_vector_direction: MotionVectorDirection,
    /// Must be false after camera cuts, resize or any temporal-history reset.
    pub previous_frame_valid: bool,
}

/// Rejection reasons for incomplete or ambiguous temporal metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalMetadataError {
    ZeroRenderExtent,
    ZeroMotionVectorExtent,
    MotionVectorExtentMismatch,
    InvalidMotionVectorScale,
    InvalidJitter,
    MissingPreviousFrame,
}

/// Validate metadata without guessing motion-vector conventions or modifying
/// renderer resources. This is a CPU-side contract, not proof that the supplied
/// vectors are accurate or that a previous-frame history texture exists.
pub fn validate_temporal_frame_metadata(
    metadata: TemporalFrameMetadata,
) -> Result<(), TemporalMetadataError> {
    if metadata.render_width == 0 || metadata.render_height == 0 {
        return Err(TemporalMetadataError::ZeroRenderExtent);
    }
    if metadata.motion_vector_width == 0 || metadata.motion_vector_height == 0 {
        return Err(TemporalMetadataError::ZeroMotionVectorExtent);
    }
    if metadata.motion_vector_width != metadata.render_width
        || metadata.motion_vector_height != metadata.render_height
    {
        return Err(TemporalMetadataError::MotionVectorExtentMismatch);
    }
    if !metadata.motion_vector_scale_x.is_finite()
        || !metadata.motion_vector_scale_y.is_finite()
        || metadata.motion_vector_scale_x == 0.0
        || metadata.motion_vector_scale_y == 0.0
    {
        return Err(TemporalMetadataError::InvalidMotionVectorScale);
    }
    if !metadata.jitter_x_pixels.is_finite() || !metadata.jitter_y_pixels.is_finite() {
        return Err(TemporalMetadataError::InvalidJitter);
    }
    if !metadata.previous_frame_valid {
        return Err(TemporalMetadataError::MissingPreviousFrame);
    }
    Ok(())
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


/// Stable slots in the future shared shader-visible DLSS descriptor table.
///
/// D3D12 permits only one CBV/SRV/UAV heap to be bound at a time. Keeping the
/// three input SRVs and output UAV in one table avoids replacing the input
/// heap when the output heap is bound. This is a layout contract only: it
/// creates no descriptors and does not prove resource states or lifetimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DlssDescriptorSlot {
    SceneColor = 0,
    Depth = 1,
    MotionVectors = 2,
    Output = 3,
}

/// Fixed descriptor-table layout for one DLSS evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DlssDescriptorTableLayout;

impl DlssDescriptorTableLayout {
    pub const DESCRIPTOR_COUNT: u32 = 4;

    pub const fn slot(self, slot: DlssDescriptorSlot) -> u32 {
        slot as u32
    }

    /// Byte offset of a descriptor from the heap's CPU/GPU start handle.
    ///
    /// The caller supplies the device-specific descriptor increment returned
    /// by D3D12. None means the increment is zero or multiplication overflowed.
    /// This does not create a heap or prove that the heap contains the slot.
    pub const fn byte_offset(
        self,
        slot: DlssDescriptorSlot,
        descriptor_increment: u32,
    ) -> Option<u32> {
        if descriptor_increment == 0 {
            return None;
        }
        (slot as u32).checked_mul(descriptor_increment)
    }

    /// Whether a descriptor index belongs to this four-entry table.
    pub const fn contains(self, descriptor_index: u32) -> bool {
        descriptor_index < Self::DESCRIPTOR_COUNT
    }
}

/// Pixel format for a dedicated DLSS output texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DlssOutputFormat {
    R8G8B8A8Unorm,
    R10G10B10A2Unorm,
    R16G16B16A16Float,
}

impl DlssOutputFormat {
    /// DXGI_FORMAT numeric value, kept independent of the Windows bindings so
    /// output sizing can be validated on every CI platform.
    pub const fn dxgi_format(self) -> u32 {
        match self {
            Self::R8G8B8A8Unorm => 28,
            Self::R10G10B10A2Unorm => 24,
            Self::R16G16B16A16Float => 10,
        }
    }

    pub const fn bytes_per_pixel(self) -> u64 {
        match self {
            Self::R8G8B8A8Unorm | Self::R10G10B10A2Unorm => 4,
            Self::R16G16B16A16Float => 8,
        }
    }
}

/// Validated dimensions and format for a dedicated 2D DLSS output texture.
/// This is an allocation request, not evidence that a GPU resource exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DlssOutputTargetDesc {
    pub width: u32,
    pub height: u32,
    pub format: DlssOutputFormat,
    pub estimated_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DlssOutputTargetDescError {
    ZeroExtent,
    ExtentTooLarge,
    AllocationSizeOverflow,
}

impl DlssOutputTargetDesc {
    pub fn new(
        width: u32,
        height: u32,
        format: DlssOutputFormat,
    ) -> Result<Self, DlssOutputTargetDescError> {
        const MAX_TEXTURE_DIMENSION: u32 = 16_384;
        if width == 0 || height == 0 {
            return Err(DlssOutputTargetDescError::ZeroExtent);
        }
        if width > MAX_TEXTURE_DIMENSION || height > MAX_TEXTURE_DIMENSION {
            return Err(DlssOutputTargetDescError::ExtentTooLarge);
        }
        let estimated_bytes = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(format.bytes_per_pixel()))
            .ok_or(DlssOutputTargetDescError::AllocationSizeOverflow)?;
        Ok(Self {
            width,
            height,
            format,
            estimated_bytes,
        })
    }
}

/// Decision for reusing or allocating the dedicated DLSS output texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DlssOutputAllocationAction {
    ReuseExisting,
    AllocateNew,
}

/// Conservative allocation-planning errors. The estimate is a lower bound;
/// the caller must still handle a real D3D12 allocation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DlssOutputAllocationError {
    BudgetUnavailable,
    InsufficientBudget { required_bytes: u64, available_bytes: u64 },
    BudgetEstimateOverflow,
}

/// Result of checking whether a requested output target can be allocated
/// without exceeding the adapter's reported *available* local-memory budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DlssOutputAllocationPlan {
    pub action: DlssOutputAllocationAction,
    pub estimated_required_bytes: u64,
    pub available_bytes: Option<u64>,
}

impl DlssOutputAllocationPlan {
    /// Reserve 25% headroom above the pixel payload estimate for driver
    /// allocation granularity and bookkeeping. This is a conservative
    /// planning margin, not an exact prediction of DXGI residency cost.
    pub fn new(
        current: Option<DlssOutputTargetDesc>,
        requested: DlssOutputTargetDesc,
        available_bytes: Option<u64>,
    ) -> Result<Self, DlssOutputAllocationError> {
        if current == Some(requested) {
            return Ok(Self {
                action: DlssOutputAllocationAction::ReuseExisting,
                estimated_required_bytes: 0,
                available_bytes,
            });
        }
        let estimated_required_bytes = requested
            .estimated_bytes
            .checked_mul(5)
            .and_then(|bytes| bytes.checked_add(3))
            .map(|bytes| bytes / 4)
            .ok_or(DlssOutputAllocationError::BudgetEstimateOverflow)?;
        let available = available_bytes.ok_or(DlssOutputAllocationError::BudgetUnavailable)?;
        if estimated_required_bytes > available {
            return Err(DlssOutputAllocationError::InsufficientBudget {
                required_bytes: estimated_required_bytes,
                available_bytes: available,
            });
        }
        Ok(Self {
            action: DlssOutputAllocationAction::AllocateNew,
            estimated_required_bytes,
            available_bytes,
        })
    }
}

/// Safe CPU-side decision for using or replacing a dedicated DLSS output target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DlssOutputTargetUseAction {
    /// The matching resource's previous GPU use has completed.
    ReuseNow,
    /// The matching resource is still in use; wait for this submission fence value.
    WaitForSubmission,
    /// Allocate a replacement and retire the old resource only after its last use.
    AllocateReplacement,
}

/// Lifetime decision for a target that may still be referenced by submitted GPU work.
///
/// Submission values must come from the same host fence timeline. This planner
/// does not wait, allocate, release COM objects or establish cross-queue ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DlssOutputTargetLifetimePlan {
    pub action: DlssOutputTargetUseAction,
    pub wait_for_submission: Option<u64>,
    pub retire_previous_after_submission: Option<u64>,
    pub allocation: Option<DlssOutputAllocationPlan>,
}

pub fn plan_dlss_output_target_lifetime(
    current: Option<DlssOutputTargetDesc>,
    requested: DlssOutputTargetDesc,
    current_last_submission: u64,
    completed_submission: u64,
    available_bytes: Option<u64>,
) -> Result<DlssOutputTargetLifetimePlan, DlssOutputAllocationError> {
    if current == Some(requested) {
        if current_last_submission > completed_submission {
            return Ok(DlssOutputTargetLifetimePlan {
                action: DlssOutputTargetUseAction::WaitForSubmission,
                wait_for_submission: Some(current_last_submission),
                retire_previous_after_submission: None,
                allocation: None,
            });
        }
        return Ok(DlssOutputTargetLifetimePlan {
            action: DlssOutputTargetUseAction::ReuseNow,
            wait_for_submission: None,
            retire_previous_after_submission: None,
            allocation: Some(DlssOutputAllocationPlan::new(
                current, requested, available_bytes,
            )?),
        });
    }

    let allocation = DlssOutputAllocationPlan::new(
        current, requested, available_bytes,
    )?;
    Ok(DlssOutputTargetLifetimePlan {
        action: DlssOutputTargetUseAction::AllocateReplacement,
        wait_for_submission: None,
        retire_previous_after_submission: current
            .and_then(|_| (current_last_submission > completed_submission)
                .then_some(current_last_submission)),
        allocation: Some(allocation),
    })
}

#[cfg(test)]
mod dlss_output_target_lifetime_tests {
    use super::{
        plan_dlss_output_target_lifetime, DlssOutputTargetDesc as Desc,
        DlssOutputTargetUseAction as Action, DlssOutputFormat as Format,
    };

    fn target(width: u32) -> Desc {
        Desc::new(width, 1080, Format::R8G8B8A8Unorm).unwrap()
    }

    #[test]
    fn matching_target_waits_for_its_last_gpu_submission() {
        let desc = target(1920);
        let plan = plan_dlss_output_target_lifetime(
            Some(desc), desc, 42, 41, None,
        ).unwrap();
        assert_eq!(plan.action, Action::WaitForSubmission);
        assert_eq!(plan.wait_for_submission, Some(42));
        assert_eq!(plan.retire_previous_after_submission, None);
        assert_eq!(plan.allocation, None);
    }

    #[test]
    fn matching_target_is_reused_after_its_submission_completes() {
        let desc = target(1920);
        let plan = plan_dlss_output_target_lifetime(
            Some(desc), desc, 42, 42, None,
        ).unwrap();
        assert_eq!(plan.action, Action::ReuseNow);
        assert_eq!(plan.wait_for_submission, None);
        assert_eq!(plan.allocation.unwrap().estimated_required_bytes, 0);
    }

    #[test]
    fn replacement_keeps_old_target_until_its_last_submission_completes() {
        let old = target(1920);
        let new = target(2560);
        let budget = new.estimated_bytes * 5 / 4;
        let plan = plan_dlss_output_target_lifetime(
            Some(old), new, 90, 89, Some(budget),
        ).unwrap();
        assert_eq!(plan.action, Action::AllocateReplacement);
        assert_eq!(plan.retire_previous_after_submission, Some(90));
        assert_eq!(plan.allocation.unwrap().action, super::DlssOutputAllocationAction::AllocateNew);
    }

    #[test]
    fn completed_old_target_needs_no_deferred_retirement() {
        let old = target(1920);
        let new = target(1280);
        let budget = new.estimated_bytes * 5 / 4;
        let plan = plan_dlss_output_target_lifetime(
            Some(old), new, 90, 90, Some(budget),
        ).unwrap();
        assert_eq!(plan.action, Action::AllocateReplacement);
        assert_eq!(plan.retire_previous_after_submission, None);
    }

    #[test]
    fn replacement_still_obeys_live_budget_policy() {
        let old = target(1920);
        let new = target(2560);
        assert!(plan_dlss_output_target_lifetime(
            Some(old), new, 90, 89, Some(1),
        ).is_err());
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
    /// Number of array slices in the D3D12 texture resource.
    pub array_size: u32,
    /// Raw D3D12_RESOURCE_FLAGS bits from the host resource descriptor.
    pub flags: u32,
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
    ShaderResourceDenied,
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
    const DENY_SHADER_RESOURCE: u32 = 0x8;
    if color.flags & DENY_SHADER_RESOURCE != 0 || depth.flags & DENY_SHADER_RESOURCE != 0 {
        return DepthPairStatus::ShaderResourceDenied;
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


/// A validated, CPU-side proposal for moving a completed resource into a
/// shader-resource state. This deliberately does not create or submit a
/// D3D12 barrier; the caller must still prove queue ownership and ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShaderReadTransitionPlan {
    pub resource_id: usize,
    pub before_state: u32,
    pub after_state: u32,
    pub recording_list: usize,
    pub submission: u64,
    pub barrier_required: bool,
}

/// Why a proposed shader-read transition was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderReadTransitionError {
    NullResource,
    NullRecordingList,
    RecordingListMismatch,
    Unsubmitted,
    SubmissionNotCompleted,
    UnknownBeforeState,
    InvalidShaderReadTarget,
}

/// Validate the identity and completion metadata before a future renderer
/// integration is allowed to construct a D3D12 transition barrier.
///
/// This is a preflight contract only. In particular, completion of one host
/// submission does not prove that another queue has not changed the resource.
pub fn plan_shader_read_transition(
    resource_id: usize,
    recording_list: usize,
    expected_recording_list: usize,
    submission: u64,
    finished_submission: u64,
    before_state: u32,
    after_state: u32,
) -> Result<ShaderReadTransitionPlan, ShaderReadTransitionError> {
    if resource_id == 0 {
        return Err(ShaderReadTransitionError::NullResource);
    }
    if recording_list == 0 || expected_recording_list == 0 {
        return Err(ShaderReadTransitionError::NullRecordingList);
    }
    if recording_list != expected_recording_list {
        return Err(ShaderReadTransitionError::RecordingListMismatch);
    }
    if submission == 0 {
        return Err(ShaderReadTransitionError::Unsubmitted);
    }
    if submission > finished_submission {
        return Err(ShaderReadTransitionError::SubmissionNotCompleted);
    }
    if matches!(classify_host_resource_state(before_state), HostResourceStateClass::Unknown) {
        return Err(ShaderReadTransitionError::UnknownBeforeState);
    }
    if !is_shader_resource_readable(after_state) {
        return Err(ShaderReadTransitionError::InvalidShaderReadTarget);
    }

    Ok(ShaderReadTransitionPlan {
        resource_id,
        before_state,
        after_state,
        recording_list,
        submission,
        barrier_required: before_state != after_state,
    })
}

/// Validate the identity and completion metadata before proposing a transition
/// into D3D12_UNORDERED_ACCESS. This does not record a barrier or prove
/// cross-queue ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnorderedAccessTransitionError {
    NullResource,
    NullRecordingList,
    RecordingListMismatch,
    Unsubmitted,
    SubmissionNotCompleted,
    UnknownBeforeState,
}

/// CPU-only proposal for making the dedicated DLSS output available as a UAV.
pub fn plan_unordered_access_transition(
    resource_id: usize,
    recording_list: usize,
    expected_recording_list: usize,
    submission: u64,
    finished_submission: u64,
    before_state: u32,
) -> Result<ShaderReadTransitionPlan, UnorderedAccessTransitionError> {
    const UNORDERED_ACCESS: u32 = 0x8;
    if resource_id == 0 {
        return Err(UnorderedAccessTransitionError::NullResource);
    }
    if recording_list == 0 || expected_recording_list == 0 {
        return Err(UnorderedAccessTransitionError::NullRecordingList);
    }
    if recording_list != expected_recording_list {
        return Err(UnorderedAccessTransitionError::RecordingListMismatch);
    }
    if submission == 0 {
        return Err(UnorderedAccessTransitionError::Unsubmitted);
    }
    if submission > finished_submission {
        return Err(UnorderedAccessTransitionError::SubmissionNotCompleted);
    }
    if matches!(classify_host_resource_state(before_state), HostResourceStateClass::Unknown) {
        return Err(UnorderedAccessTransitionError::UnknownBeforeState);
    }
    Ok(ShaderReadTransitionPlan {
        resource_id,
        before_state,
        after_state: UNORDERED_ACCESS,
        recording_list,
        submission,
        barrier_required: before_state != UNORDERED_ACCESS,
    })
}

/// All CPU-side evidence required before a future Streamline DLSS SR call.
///
/// The resource identifiers and state values must come from the exact completed
/// guest frame. This function only validates and assembles transition
/// proposals; it does not record barriers, prove cross-queue ownership, tag
/// Streamline resources or evaluate DLSS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DlssEvaluationInputs {
    pub frame: TemporalFramePlan,
    pub resolution: ResolutionPlan,
    pub available_inputs: FrameInputs,
    pub temporal_metadata: TemporalFrameMetadata,
    /// Actual host texture metadata captured for this exact completed frame.
    pub color_texture: TextureInfo,
    pub depth_texture: TextureInfo,
    pub motion_vectors_texture: TextureInfo,
    /// Dedicated output texture at the final presentation resolution.
    pub output_texture: TextureInfo,
    pub depth_view_format: u32,
    pub depth_view_dimension: u32,
    pub color_resource_id: usize,
    pub depth_resource_id: usize,
    pub motion_vectors_resource_id: usize,
    pub output_resource_id: usize,
    pub recording_list: usize,
    pub expected_recording_list: usize,
    pub submission: u64,
    pub finished_submission: u64,
    pub color_before_state: u32,
    pub depth_before_state: u32,
    pub motion_vectors_before_state: u32,
    /// Tracked state of the dedicated output resource before DLSS evaluation.
    pub output_before_state: u32,
}

/// Why a future DLSS evaluation cannot be prepared from this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DlssEvaluationPlanError {
    MissingRequiredInputs,
    RenderExtentMismatch,
    InvalidColorDepthPair(DepthPairStatus),
    MotionVectorTextureMismatch,
    MotionVectorsShaderResourceDenied,
    OutputTextureMismatch,
    OutputNotUnorderedAccessCapable,
    MultisampledInput,
    TextureArrayNotSupported,
    OutputIsNotLargerThanRender,
    JitterDoesNotMatchFrame,
    TemporalHistoryUnavailable,
    AliasedResources,
    InvalidTemporalMetadata(TemporalMetadataError),
    InvalidResourceTransition(ShaderReadTransitionError),
    InvalidOutputTransition(UnorderedAccessTransitionError),
}

/// Validated CPU-side input and transition proposal for one DLSS frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DlssEvaluationPlan {
    pub frame_index: u64,
    pub render_width: u32,
    pub render_height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub output_resource_id: usize,
    pub jitter: TemporalJitter,
    pub reset_history: bool,
    pub color_transition: ShaderReadTransitionPlan,
    pub depth_transition: ShaderReadTransitionPlan,
    pub motion_vectors_transition: ShaderReadTransitionPlan,
    pub output_transition: ShaderReadTransitionPlan,
}

/// Assemble a conservative evaluation plan for a single completed frame.
///
/// The first frame after a reset is deliberately rejected until valid history
/// is available. A later Streamline integration can use the reset signal and
/// initialize history resources explicitly rather than guessing.
pub fn plan_dlss_evaluation(
    inputs: DlssEvaluationInputs,
) -> Result<DlssEvaluationPlan, DlssEvaluationPlanError> {
    if !inputs.available_inputs.ready() {
        return Err(DlssEvaluationPlanError::MissingRequiredInputs);
    }

    let frame = inputs.frame;
    let resolution = inputs.resolution;
    let metadata = inputs.temporal_metadata;

    if resolution.render_width != frame.render_width
        || resolution.render_height != frame.render_height
        || metadata.render_width != frame.render_width
        || metadata.render_height != frame.render_height
    {
        return Err(DlssEvaluationPlanError::RenderExtentMismatch);
    }
    let color_depth_status = validate_depth_pair(
        inputs.color_texture,
        inputs.depth_texture,
        inputs.depth_view_format,
        inputs.depth_view_dimension,
    );
    if color_depth_status != DepthPairStatus::Compatible {
        return Err(DlssEvaluationPlanError::InvalidColorDepthPair(color_depth_status));
    }
    // Streamline DLSS SR consumes resolved 2D inputs. Never pass an MSAA
    // render target directly; the renderer must resolve it into single-sample
    // scene-color/depth/motion-vector textures first.
    if inputs.color_texture.samples != 1
        || inputs.depth_texture.samples != 1
        || inputs.motion_vectors_texture.samples != 1
    {
        return Err(DlssEvaluationPlanError::MultisampledInput);
    }
    // The current input contract is for ordinary Texture2D resources, not
    // texture arrays. Streamline view tagging must not silently select a slice.
    if inputs.color_texture.array_size != 1
        || inputs.depth_texture.array_size != 1
        || inputs.motion_vectors_texture.array_size != 1
    {
        return Err(DlssEvaluationPlanError::TextureArrayNotSupported);
    }
    let motion_vectors = inputs.motion_vectors_texture;
    if motion_vectors.width != frame.render_width
        || motion_vectors.height != frame.render_height
        || motion_vectors.dimension != 3
        || motion_vectors.samples == 0
    {
        return Err(DlssEvaluationPlanError::MotionVectorTextureMismatch);
    }
    const DENY_SHADER_RESOURCE: u32 = 0x8;
    if motion_vectors.flags & DENY_SHADER_RESOURCE != 0 {
        return Err(DlssEvaluationPlanError::MotionVectorsShaderResourceDenied);
    }
    if resolution.output_width <= resolution.render_width
        || resolution.output_height <= resolution.render_height
    {
        return Err(DlssEvaluationPlanError::OutputIsNotLargerThanRender);
    }
    // DLSS SR needs a distinct final-resolution output target. A swap-chain
    // back buffer is not assumed to support UAV access; the renderer must
    // supply a dedicated texture whose descriptor explicitly allows UAVs.
    let output = inputs.output_texture;
    if output.width != resolution.output_width
        || output.height != resolution.output_height
        || output.dimension != 3
        || output.samples != 1
        || output.array_size != 1
    {
        return Err(DlssEvaluationPlanError::OutputTextureMismatch);
    }
    const ALLOW_UNORDERED_ACCESS: u32 = 0x4;
    if output.flags & ALLOW_UNORDERED_ACCESS == 0 {
        return Err(DlssEvaluationPlanError::OutputNotUnorderedAccessCapable);
    }
    if metadata.jitter_x_pixels != frame.jitter.x_pixels
        || metadata.jitter_y_pixels != frame.jitter.y_pixels
    {
        return Err(DlssEvaluationPlanError::JitterDoesNotMatchFrame);
    }
    if !frame.previous_frame_valid || !metadata.previous_frame_valid || frame.reset_reason.is_some() {
        return Err(DlssEvaluationPlanError::TemporalHistoryUnavailable);
    }
    validate_temporal_frame_metadata(metadata)
        .map_err(DlssEvaluationPlanError::InvalidTemporalMetadata)?;

    let resources = [
        inputs.color_resource_id,
        inputs.depth_resource_id,
        inputs.motion_vectors_resource_id,
        inputs.output_resource_id,
    ];
    if resources.contains(&0)
        || resources[0] == resources[1]
        || resources[0] == resources[2]
        || resources[1] == resources[2]
        || resources[3] == resources[0]
        || resources[3] == resources[1]
        || resources[3] == resources[2]
    {
        return Err(DlssEvaluationPlanError::AliasedResources);
    }

    const SHADER_READ: u32 = 0xC0;
    const DEPTH_SHADER_READ: u32 = 0xE0;
    let transition = |resource_id, before_state, after_state| {
        plan_shader_read_transition(
            resource_id,
            inputs.recording_list,
            inputs.expected_recording_list,
            inputs.submission,
            inputs.finished_submission,
            before_state,
            after_state,
        )
        .map_err(DlssEvaluationPlanError::InvalidResourceTransition)
    };

    // Validate the output transition first so its own failure is reported
    // directly rather than being shadowed by the same shared submission gate
    // while planning one of the input transitions.
    let output_transition = plan_unordered_access_transition(
        inputs.output_resource_id,
        inputs.recording_list,
        inputs.expected_recording_list,
        inputs.submission,
        inputs.finished_submission,
        inputs.output_before_state,
    )
    .map_err(DlssEvaluationPlanError::InvalidOutputTransition)?;

    Ok(DlssEvaluationPlan {
        frame_index: frame.frame_index,
        render_width: resolution.render_width,
        render_height: resolution.render_height,
        output_width: resolution.output_width,
        output_height: resolution.output_height,
        output_resource_id: inputs.output_resource_id,
        jitter: frame.jitter,
        reset_history: false,
        color_transition: transition(
            inputs.color_resource_id,
            inputs.color_before_state,
            SHADER_READ,
        )?,
        depth_transition: transition(
            inputs.depth_resource_id,
            inputs.depth_before_state,
            DEPTH_SHADER_READ,
        )?,
        motion_vectors_transition: transition(
            inputs.motion_vectors_resource_id,
            inputs.motion_vectors_before_state,
            SHADER_READ,
        )?,
        output_transition,
    })
}

#[cfg(test)]
mod dlss_evaluation_plan_tests {
    use super::{
        plan_dlss_evaluation, DepthPairStatus, DlssEvaluationInputs, DlssEvaluationPlanError as Error,
        FrameInputs, MotionVectorDirection, MotionVectorUnits, ResolutionPlan,
        TextureInfo, TemporalFrameMetadata, TemporalHistoryResetReason as Reset, TemporalMetadataError,
        TemporalHistoryTracker as Tracker, ShaderReadTransitionError,
    };

    fn valid_inputs() -> DlssEvaluationInputs {
        let mut tracker = Tracker::default();
        tracker.plan_frame(1, 1280, 720, false, false).unwrap();
        let frame = tracker.plan_frame(2, 1280, 720, false, false).unwrap();
        DlssEvaluationInputs {
            frame,
            resolution: ResolutionPlan {
                render_width: 1280,
                render_height: 720,
                output_width: 1920,
                output_height: 1080,
            },
            available_inputs: FrameInputs {
                color: true,
                depth: true,
                motion_vectors: true,
                jitter: true,
                exposure: false,
            },
            temporal_metadata: TemporalFrameMetadata {
                render_width: 1280,
                render_height: 720,
                motion_vector_width: 1280,
                motion_vector_height: 720,
                motion_vector_scale_x: 1.0,
                motion_vector_scale_y: 1.0,
                jitter_x_pixels: frame.jitter.x_pixels,
                jitter_y_pixels: frame.jitter.y_pixels,
                motion_vector_units: MotionVectorUnits::Pixels,
                motion_vector_direction: MotionVectorDirection::CurrentToPrevious,
                previous_frame_valid: true,
            },
            color_texture: TextureInfo { width: 1280, height: 720, samples: 1, dimension: 3, array_size: 1, flags: 0 },
            depth_texture: TextureInfo { width: 1280, height: 720, samples: 1, dimension: 3, array_size: 1, flags: 0 },
            motion_vectors_texture: TextureInfo { width: 1280, height: 720, samples: 1, dimension: 3, array_size: 1, flags: 0 },
            output_texture: TextureInfo { width: 1920, height: 1080, samples: 1, dimension: 3, array_size: 1, flags: 0x4 },
            depth_view_format: 40,
            depth_view_dimension: 3,
            color_resource_id: 0x100,
            depth_resource_id: 0x200,
            motion_vectors_resource_id: 0x300,
            output_resource_id: 0x400,
            recording_list: 0xabc,
            expected_recording_list: 0xabc,
            submission: 9,
            finished_submission: 9,
            color_before_state: 0x4,
            depth_before_state: 0x10,
            motion_vectors_before_state: 0x8,
            output_before_state: 0,
        }
    }

    #[test]
    fn complete_frame_produces_three_shader_read_transition_proposals() {
        let plan = plan_dlss_evaluation(valid_inputs()).unwrap();
        assert_eq!(plan.frame_index, 2);
        assert_eq!((plan.render_width, plan.render_height), (1280, 720));
        assert_eq!((plan.output_width, plan.output_height), (1920, 1080));
        assert_eq!(plan.output_resource_id, 0x400);
        assert_eq!(plan.color_transition.after_state, 0xC0);
        assert_eq!(plan.depth_transition.after_state, 0xE0);
        assert_eq!(plan.motion_vectors_transition.after_state, 0xC0);
        assert_eq!(plan.output_transition.after_state, 0x8);
        assert_eq!(plan.output_transition.resource_id, 0x400);
        assert!(plan.output_transition.barrier_required);
        assert!(plan.color_transition.barrier_required);
        assert!(!plan.reset_history);
    }

    #[test]
    fn output_transition_requires_known_state_and_completed_exact_submission() {
        let mut inputs = valid_inputs();
        inputs.output_before_state = 0x8000_0000;
        assert!(matches!(
            plan_dlss_evaluation(inputs),
            Err(Error::InvalidOutputTransition(
                super::UnorderedAccessTransitionError::UnknownBeforeState
            ))
        ));

        let mut inputs = valid_inputs();
        inputs.finished_submission = inputs.submission - 1;
        assert!(matches!(
            plan_dlss_evaluation(inputs),
            Err(Error::InvalidOutputTransition(
                super::UnorderedAccessTransitionError::SubmissionNotCompleted
            ))
        ));
    }

    #[test]
    fn mismatched_color_depth_extent_and_depth_format_are_rejected() {
        let mut inputs = valid_inputs();
        inputs.depth_texture.width = 640;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::InvalidColorDepthPair(super::DepthPairStatus::ExtentMismatch))
        );

        let mut inputs = valid_inputs();
        inputs.depth_view_format = 0;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::InvalidColorDepthPair(super::DepthPairStatus::UnsupportedDepthFormat))
        );
    }

    #[test]
    fn motion_vector_texture_must_match_render_extent_and_be_2d() {
        let mut inputs = valid_inputs();
        inputs.motion_vectors_texture.height = 360;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::MotionVectorTextureMismatch));

        let mut inputs = valid_inputs();
        inputs.motion_vectors_texture.dimension = 4;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::MotionVectorTextureMismatch));
    }

    #[test]
    fn multisampled_inputs_are_rejected_until_resolved() {
        let mut inputs = valid_inputs();
        inputs.color_texture.samples = 4;
        inputs.depth_texture.samples = 4;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::MultisampledInput));

        let mut inputs = valid_inputs();
        inputs.motion_vectors_texture.samples = 2;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::MultisampledInput));
    }

    #[test]
    fn output_target_must_be_final_resolution_single_sample_2d_and_uav_capable() {
        let mut inputs = valid_inputs();
        inputs.output_texture.width = 1280;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::OutputTextureMismatch));

        let mut inputs = valid_inputs();
        inputs.output_texture.samples = 4;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::OutputTextureMismatch));

        let mut inputs = valid_inputs();
        inputs.output_texture.array_size = 2;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::OutputTextureMismatch));

        let mut inputs = valid_inputs();
        inputs.output_texture.flags = 0;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::OutputNotUnorderedAccessCapable));

        let mut inputs = valid_inputs();
        inputs.output_resource_id = inputs.color_resource_id;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::AliasedResources));
    }

    #[test]
    fn resources_that_deny_shader_resource_views_are_rejected() {
        let mut inputs = valid_inputs();
        inputs.color_texture.flags = 0x8;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::InvalidColorDepthPair(DepthPairStatus::ShaderResourceDenied))
        );

        let mut inputs = valid_inputs();
        inputs.depth_texture.flags = 0x8;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::InvalidColorDepthPair(DepthPairStatus::ShaderResourceDenied))
        );

        let mut inputs = valid_inputs();
        inputs.motion_vectors_texture.flags = 0x8;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::MotionVectorsShaderResourceDenied));
    }

    #[test]
    fn texture_arrays_are_rejected_until_slice_views_are_explicit() {
        let mut inputs = valid_inputs();
        inputs.color_texture.array_size = 2;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::TextureArrayNotSupported));

        let mut inputs = valid_inputs();
        inputs.motion_vectors_texture.array_size = 4;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::TextureArrayNotSupported));
    }

    #[test]
    fn missing_required_input_is_rejected() {
        let mut inputs = valid_inputs();
        inputs.available_inputs.motion_vectors = false;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::MissingRequiredInputs)
        );
    }

    #[test]
    fn mismatched_extent_and_jitter_are_rejected() {
        let mut inputs = valid_inputs();
        inputs.temporal_metadata.render_width = 640;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::RenderExtentMismatch));

        let mut inputs = valid_inputs();
        inputs.temporal_metadata.jitter_x_pixels += 0.125;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::JitterDoesNotMatchFrame));
    }

    #[test]
    fn first_frame_or_reset_cannot_be_evaluated_with_missing_history() {
        let mut inputs = valid_inputs();
        inputs.frame.previous_frame_valid = false;
        inputs.frame.reset_reason = Some(Reset::InitialFrame);
        inputs.temporal_metadata.previous_frame_valid = false;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::TemporalHistoryUnavailable)
        );
    }

    #[test]
    fn resource_aliasing_is_rejected() {
        let mut inputs = valid_inputs();
        inputs.motion_vectors_resource_id = inputs.depth_resource_id;
        assert_eq!(plan_dlss_evaluation(inputs), Err(Error::AliasedResources));
    }

    #[test]
    fn incomplete_submission_and_unknown_states_are_rejected() {
        let mut inputs = valid_inputs();
        inputs.finished_submission = 8;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::InvalidOutputTransition(
                super::UnorderedAccessTransitionError::SubmissionNotCompleted
            ))
        );

        let mut inputs = valid_inputs();
        inputs.color_before_state = 0xFFFF;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::InvalidResourceTransition(
                ShaderReadTransitionError::UnknownBeforeState
            ))
        );
    }

    #[test]
    fn invalid_motion_vector_contract_is_rejected() {
        let mut inputs = valid_inputs();
        inputs.temporal_metadata.motion_vector_width = 640;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::InvalidTemporalMetadata(TemporalMetadataError::MotionVectorExtentMismatch))
        );
        let mut inputs = valid_inputs();
        inputs.temporal_metadata.motion_vector_scale_x = f32::NAN;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::InvalidTemporalMetadata(TemporalMetadataError::InvalidMotionVectorScale))
        );
    }

    #[test]
    fn non_upscaled_output_extent_is_rejected() {
        let mut inputs = valid_inputs();
        inputs.resolution.output_width = inputs.resolution.render_width;
        assert_eq!(
            plan_dlss_evaluation(inputs),
            Err(Error::OutputIsNotLargerThanRender)
        );
    }
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
        let color = TextureInfo { width: 1920, height: 1080, samples: 1, dimension: 3, array_size: 1, flags: 0 };
        let depth = color;
        assert_eq!(validate_depth_pair(color, depth, 40, 3), DepthPairStatus::Compatible);
        assert_eq!(validate_depth_pair(TextureInfo { flags: 0x8, ..color }, depth, 40, 3), DepthPairStatus::ShaderResourceDenied);
        assert_eq!(validate_depth_pair(color, TextureInfo { flags: 0x8, ..depth }, 40, 3), DepthPairStatus::ShaderResourceDenied);
        assert_eq!(validate_depth_pair(color, TextureInfo { width: 1280, ..depth }, 40, 3), DepthPairStatus::ExtentMismatch);
        assert_eq!(validate_depth_pair(color, TextureInfo { samples: 4, ..depth }, 40, 3), DepthPairStatus::SampleCountMismatch);
        assert_eq!(validate_depth_pair(color, depth, 39, 3), DepthPairStatus::UnsupportedDepthFormat);
        assert_eq!(validate_depth_pair(color, depth, 40, 4), DepthPairStatus::NotTexture2D);
        assert_eq!(validate_depth_pair(color, TextureInfo { width: 0, ..depth }, 40, 3), DepthPairStatus::MissingExtent);
    }

}


#[cfg(test)]
mod shader_read_transition_tests {
    use super::{
        plan_shader_read_transition, ShaderReadTransitionError as Error,
    };

    const PIXEL_READ: u32 = 0x80;
    const NON_PIXEL_READ: u32 = 0x40;
    const RENDER_TARGET: u32 = 0x4;

    #[test]
    fn completed_matching_submission_produces_a_transition_plan() {
        let plan = plan_shader_read_transition(
            0x100, 0xabc, 0xabc, 12, 12, RENDER_TARGET,
            PIXEL_READ | NON_PIXEL_READ,
        ).unwrap();
        assert_eq!(plan.resource_id, 0x100);
        assert_eq!(plan.before_state, RENDER_TARGET);
        assert_eq!(plan.after_state, PIXEL_READ | NON_PIXEL_READ);
        assert!(plan.barrier_required);
    }

    #[test]
    fn already_shader_readable_state_does_not_need_a_barrier() {
        let plan = plan_shader_read_transition(
            0x100, 0xabc, 0xabc, 12, 12, PIXEL_READ, PIXEL_READ,
        ).unwrap();
        assert!(!plan.barrier_required);
    }

    #[test]
    fn rejects_null_resource_and_recording_list() {
        assert_eq!(
            plan_shader_read_transition(0, 0xabc, 0xabc, 1, 1, 0, PIXEL_READ),
            Err(Error::NullResource)
        );
        assert_eq!(
            plan_shader_read_transition(0x100, 0, 0, 1, 1, 0, PIXEL_READ),
            Err(Error::NullRecordingList)
        );
    }

    #[test]
    fn rejects_a_different_recording_list() {
        assert_eq!(
            plan_shader_read_transition(0x100, 0xabc, 0xdef, 1, 1, 0, PIXEL_READ),
            Err(Error::RecordingListMismatch)
        );
    }

    #[test]
    fn rejects_unsubmitted_and_incomplete_submissions() {
        assert_eq!(
            plan_shader_read_transition(0x100, 0xabc, 0xabc, 0, 0, 0, PIXEL_READ),
            Err(Error::Unsubmitted)
        );
        assert_eq!(
            plan_shader_read_transition(0x100, 0xabc, 0xabc, 9, 8, 0, PIXEL_READ),
            Err(Error::SubmissionNotCompleted)
        );
    }

    #[test]
    fn rejects_unknown_source_and_non_shader_target_states() {
        assert_eq!(
            plan_shader_read_transition(0x100, 0xabc, 0xabc, 1, 1, 0x1000, PIXEL_READ),
            Err(Error::UnknownBeforeState)
        );
        assert_eq!(
            plan_shader_read_transition(0x100, 0xabc, 0xabc, 1, 1, 0, RENDER_TARGET),
            Err(Error::InvalidShaderReadTarget)
        );
    }
}



#[cfg(test)]
mod temporal_metadata_tests {
    use super::{
        validate_temporal_frame_metadata, MotionVectorDirection, MotionVectorUnits,
        TemporalFrameMetadata, TemporalMetadataError as Error,
    };

    fn valid_metadata() -> TemporalFrameMetadata {
        TemporalFrameMetadata {
            render_width: 1920,
            render_height: 1080,
            motion_vector_width: 1920,
            motion_vector_height: 1080,
            motion_vector_scale_x: 1.0,
            motion_vector_scale_y: 1.0,
            jitter_x_pixels: 0.25,
            jitter_y_pixels: -0.25,
            motion_vector_units: MotionVectorUnits::Pixels,
            motion_vector_direction: MotionVectorDirection::CurrentToPrevious,
            previous_frame_valid: true,
        }
    }

    #[test]
    fn accepts_complete_finite_temporal_metadata() {
        assert_eq!(validate_temporal_frame_metadata(valid_metadata()), Ok(()));
    }

    #[test]
    fn rejects_zero_render_or_motion_vector_extent() {
        let mut metadata = valid_metadata();
        metadata.render_width = 0;
        assert_eq!(validate_temporal_frame_metadata(metadata), Err(Error::ZeroRenderExtent));

        let mut metadata = valid_metadata();
        metadata.motion_vector_height = 0;
        assert_eq!(validate_temporal_frame_metadata(metadata), Err(Error::ZeroMotionVectorExtent));
    }

    #[test]
    fn rejects_motion_vector_extent_mismatch() {
        let mut metadata = valid_metadata();
        metadata.motion_vector_width = 960;
        assert_eq!(
            validate_temporal_frame_metadata(metadata),
            Err(Error::MotionVectorExtentMismatch)
        );
    }

    #[test]
    fn rejects_zero_or_non_finite_motion_vector_scale() {
        let mut metadata = valid_metadata();
        metadata.motion_vector_scale_x = 0.0;
        assert_eq!(
            validate_temporal_frame_metadata(metadata),
            Err(Error::InvalidMotionVectorScale)
        );

        let mut metadata = valid_metadata();
        metadata.motion_vector_scale_y = f32::INFINITY;
        assert_eq!(
            validate_temporal_frame_metadata(metadata),
            Err(Error::InvalidMotionVectorScale)
        );
    }

    #[test]
    fn rejects_non_finite_jitter() {
        let mut metadata = valid_metadata();
        metadata.jitter_x_pixels = f32::NAN;
        assert_eq!(validate_temporal_frame_metadata(metadata), Err(Error::InvalidJitter));
    }

    #[test]
    fn rejects_missing_previous_frame_history() {
        let mut metadata = valid_metadata();
        metadata.previous_frame_valid = false;
        assert_eq!(
            validate_temporal_frame_metadata(metadata),
            Err(Error::MissingPreviousFrame)
        );
    }

    #[test]
    fn accepts_explicit_normalized_vectors_and_reverse_direction() {
        let mut metadata = valid_metadata();
        metadata.motion_vector_units = MotionVectorUnits::Normalized;
        metadata.motion_vector_direction = MotionVectorDirection::PreviousToCurrent;
        assert_eq!(validate_temporal_frame_metadata(metadata), Ok(()));
    }
}


/// Why temporal reconstruction history is unavailable for the current frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalHistoryResetReason {
    InitialFrame,
    RenderExtentChanged,
    CameraCut,
    ExplicitReset,
    FrameDiscontinuity,
}

/// Result of advancing the CPU-side temporal history lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemporalHistoryStatus {
    /// True only when the immediately preceding frame is a valid history source.
    pub previous_frame_valid: bool,
    /// Set when the current frame must start a new temporal history sequence.
    pub reset_reason: Option<TemporalHistoryResetReason>,
}

/// Invalid input to the temporal history lifecycle tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalHistoryError {
    ZeroRenderExtent,
}

/// Tracks frame continuity and reset conditions for a future temporal upscaler.
///
/// This is metadata bookkeeping only. It does not allocate, retain, clear or
/// validate GPU history textures, and it must not be used as proof that history
/// pixels or motion vectors are available.
#[derive(Debug, Clone, Copy, Default)]
pub struct TemporalHistoryTracker {
    previous_frame: Option<(u64, u32, u32)>,
}

impl TemporalHistoryTracker {
    /// Advance the history lifecycle for one rendered frame.
    ///
    /// Camera cuts and explicit resets invalidate the prior frame. Resize,
    /// first-frame and non-consecutive frame indices are detected automatically.
    pub fn begin_frame(
        &mut self,
        frame_index: u64,
        render_width: u32,
        render_height: u32,
        camera_cut: bool,
        explicit_reset: bool,
    ) -> Result<TemporalHistoryStatus, TemporalHistoryError> {
        if render_width == 0 || render_height == 0 {
            return Err(TemporalHistoryError::ZeroRenderExtent);
        }

        let reset_reason = match self.previous_frame {
            None => Some(TemporalHistoryResetReason::InitialFrame),
            Some((_, width, height)) if width != render_width || height != render_height => {
                Some(TemporalHistoryResetReason::RenderExtentChanged)
            }
            Some(_) if camera_cut => Some(TemporalHistoryResetReason::CameraCut),
            Some((_, _, _)) if explicit_reset => {
                Some(TemporalHistoryResetReason::ExplicitReset)
            }
            Some((previous_index, _, _))
                if previous_index.checked_add(1) != Some(frame_index) =>
            {
                Some(TemporalHistoryResetReason::FrameDiscontinuity)
            }
            Some((_, _, _)) => None,
        };

        self.previous_frame = Some((frame_index, render_width, render_height));
        Ok(TemporalHistoryStatus {
            previous_frame_valid: reset_reason.is_none(),
            reset_reason,
        })
    }

    /// Forget the tracked frame so the next call starts a fresh history sequence.
    pub fn invalidate(&mut self) {
        self.previous_frame = None;
    }
}

#[cfg(test)]
mod temporal_history_tests {
    use super::{
        TemporalHistoryError, TemporalHistoryResetReason as Reset,
        TemporalHistoryTracker,
    };

    #[test]
    fn first_frame_starts_without_previous_history() {
        let mut tracker = TemporalHistoryTracker::default();
        let status = tracker.begin_frame(1, 1920, 1080, false, false).unwrap();
        assert!(!status.previous_frame_valid);
        assert_eq!(status.reset_reason, Some(Reset::InitialFrame));
    }

    #[test]
    fn consecutive_frames_preserve_history() {
        let mut tracker = TemporalHistoryTracker::default();
        tracker.begin_frame(10, 1920, 1080, false, false).unwrap();
        let status = tracker.begin_frame(11, 1920, 1080, false, false).unwrap();
        assert!(status.previous_frame_valid);
        assert_eq!(status.reset_reason, None);
    }

    #[test]
    fn resize_invalidates_history() {
        let mut tracker = TemporalHistoryTracker::default();
        tracker.begin_frame(1, 1920, 1080, false, false).unwrap();
        let status = tracker.begin_frame(2, 1280, 720, false, false).unwrap();
        assert_eq!(status.reset_reason, Some(Reset::RenderExtentChanged));
        assert!(!status.previous_frame_valid);
    }

    #[test]
    fn camera_cut_and_explicit_reset_invalidate_history() {
        let mut tracker = TemporalHistoryTracker::default();
        tracker.begin_frame(1, 1920, 1080, false, false).unwrap();
        assert_eq!(
            tracker.begin_frame(2, 1920, 1080, true, false).unwrap().reset_reason,
            Some(Reset::CameraCut)
        );
        assert_eq!(
            tracker.begin_frame(3, 1920, 1080, false, true).unwrap().reset_reason,
            Some(Reset::ExplicitReset)
        );
        assert!(tracker.begin_frame(4, 1920, 1080, false, false).unwrap().previous_frame_valid);
    }

    #[test]
    fn skipped_or_repeated_frame_indices_invalidate_history() {
        let mut tracker = TemporalHistoryTracker::default();
        tracker.begin_frame(1, 1920, 1080, false, false).unwrap();
        assert_eq!(
            tracker.begin_frame(3, 1920, 1080, false, false).unwrap().reset_reason,
            Some(Reset::FrameDiscontinuity)
        );
        assert_eq!(
            tracker.begin_frame(3, 1920, 1080, false, false).unwrap().reset_reason,
            Some(Reset::FrameDiscontinuity)
        );
    }

    #[test]
    fn explicit_invalidation_and_zero_extent_are_handled() {
        let mut tracker = TemporalHistoryTracker::default();
        assert_eq!(
            tracker.begin_frame(1, 0, 1080, false, false),
            Err(TemporalHistoryError::ZeroRenderExtent)
        );
        tracker.begin_frame(1, 1920, 1080, false, false).unwrap();
        tracker.invalidate();
        assert_eq!(
            tracker.begin_frame(2, 1920, 1080, false, false).unwrap().reset_reason,
            Some(Reset::InitialFrame)
        );
    }
}


/// Sub-pixel projection offset for one temporal frame, in render-pixel units.
///
/// The sequence is deterministic and centered around zero. The renderer must
/// still apply this offset to its projection matrix and pass the same values
/// to the upscaler; generating the values alone does not jitter the scene.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TemporalJitter {
    pub x_pixels: f32,
    pub y_pixels: f32,
}

fn halton(index: u64, base: u64) -> f64 {
    let mut index = index;
    let mut fraction = 1.0;
    let mut value = 0.0;
    while index > 0 {
        fraction /= base as f64;
        value += fraction * (index % base) as f64;
        index /= base;
    }
    value
}

/// Generate deterministic Halton(2, 3) sub-pixel jitter for a frame.
///
/// Values are centered in the interval [-0.5, 0.5) pixels. This is only a
/// sequence generator: callers remain responsible for applying the offset
/// consistently to the camera projection and the Streamline frame constants.
pub fn temporal_jitter(frame_index: u64) -> TemporalJitter {
    let sample = frame_index.saturating_add(1);
    TemporalJitter {
        x_pixels: (halton(sample, 2) - 0.5) as f32,
        y_pixels: (halton(sample, 3) - 0.5) as f32,
    }
}

/// One coherent CPU-side temporal frame description.
///
/// The frame index, render extent, history-validity decision and jitter sample
/// are produced together so callers cannot accidentally select jitter from a
/// different frame than the history tracker. This does not apply projection
/// jitter, create motion vectors or prove that GPU history textures exist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TemporalFramePlan {
    pub frame_index: u64,
    pub render_width: u32,
    pub render_height: u32,
    pub jitter: TemporalJitter,
    pub previous_frame_valid: bool,
    pub reset_reason: Option<TemporalHistoryResetReason>,
}

impl TemporalHistoryTracker {
    /// Advance temporal history and produce the matching frame's jitter.
    pub fn plan_frame(
        &mut self,
        frame_index: u64,
        render_width: u32,
        render_height: u32,
        camera_cut: bool,
        explicit_reset: bool,
    ) -> Result<TemporalFramePlan, TemporalHistoryError> {
        let history = self.begin_frame(
            frame_index,
            render_width,
            render_height,
            camera_cut,
            explicit_reset,
        )?;
        Ok(TemporalFramePlan {
            frame_index,
            render_width,
            render_height,
            jitter: temporal_jitter(frame_index),
            previous_frame_valid: history.previous_frame_valid,
            reset_reason: history.reset_reason,
        })
    }
}

#[cfg(test)]
mod temporal_frame_plan_tests {
    use super::{
        temporal_jitter, TemporalHistoryError, TemporalHistoryResetReason as Reset,
        TemporalHistoryTracker,
    };

    #[test]
    fn frame_plan_keeps_jitter_and_history_on_the_same_frame() {
        let mut tracker = TemporalHistoryTracker::default();
        let first = tracker.plan_frame(40, 1920, 1080, false, false).unwrap();
        assert_eq!(first.frame_index, 40);
        assert_eq!((first.render_width, first.render_height), (1920, 1080));
        assert_eq!(first.jitter, temporal_jitter(40));
        assert!(!first.previous_frame_valid);
        assert_eq!(first.reset_reason, Some(Reset::InitialFrame));

        let next = tracker.plan_frame(41, 1920, 1080, false, false).unwrap();
        assert_eq!(next.jitter, temporal_jitter(41));
        assert!(next.previous_frame_valid);
        assert_eq!(next.reset_reason, None);
    }

    #[test]
    fn frame_plan_reports_resize_and_camera_cut_resets() {
        let mut tracker = TemporalHistoryTracker::default();
        tracker.plan_frame(1, 1920, 1080, false, false).unwrap();
        let resized = tracker.plan_frame(2, 1280, 720, false, false).unwrap();
        assert_eq!(resized.reset_reason, Some(Reset::RenderExtentChanged));
        assert!(!resized.previous_frame_valid);

        let cut = tracker.plan_frame(3, 1280, 720, true, false).unwrap();
        assert_eq!(cut.reset_reason, Some(Reset::CameraCut));
        assert!(!cut.previous_frame_valid);
    }

    #[test]
    fn frame_plan_reports_explicit_and_discontinuous_resets() {
        let mut tracker = TemporalHistoryTracker::default();
        tracker.plan_frame(10, 1280, 720, false, false).unwrap();
        let explicit = tracker.plan_frame(11, 1280, 720, false, true).unwrap();
        assert_eq!(explicit.reset_reason, Some(Reset::ExplicitReset));
        let skipped = tracker.plan_frame(13, 1280, 720, false, false).unwrap();
        assert_eq!(skipped.reset_reason, Some(Reset::FrameDiscontinuity));
    }

    #[test]
    fn zero_extent_is_rejected_without_advancing_the_tracker() {
        let mut tracker = TemporalHistoryTracker::default();
        assert_eq!(
            tracker.plan_frame(1, 0, 720, false, false),
            Err(TemporalHistoryError::ZeroRenderExtent)
        );
        let valid = tracker.plan_frame(1, 1280, 720, false, false).unwrap();
        assert_eq!(valid.reset_reason, Some(Reset::InitialFrame));
    }
}

#[cfg(test)]
mod temporal_jitter_tests {
    use super::{temporal_jitter, TemporalJitter};

    #[test]
    fn first_sample_is_deterministic() {
        assert_eq!(
            temporal_jitter(0),
            TemporalJitter {
                x_pixels: 0.0,
                y_pixels: -1.0 / 6.0,
            }
        );
    }

    #[test]
    fn sequence_is_deterministic_and_changes_between_frames() {
        let first = temporal_jitter(4);
        assert_eq!(first, temporal_jitter(4));
        assert_ne!(first, temporal_jitter(5));
    }

    #[test]
    fn offsets_stay_within_half_a_render_pixel() {
        for frame in 0..4096 {
            let jitter = temporal_jitter(frame);
            assert!(jitter.x_pixels >= -0.5 && jitter.x_pixels < 0.5);
            assert!(jitter.y_pixels >= -0.5 && jitter.y_pixels < 0.5);
        }
    }

    #[test]
    fn maximum_frame_index_stays_finite() {
        let jitter = temporal_jitter(u64::MAX);
        assert!(jitter.x_pixels.is_finite());
        assert!(jitter.y_pixels.is_finite());
    }
}

#[cfg(test)]
mod dlss_output_target_desc_tests {
    use super::{DlssOutputFormat as Format, DlssOutputTargetDesc as Desc, DlssOutputTargetDescError as Error};

    #[test]
    fn accepts_supported_output_formats_and_estimates_memory() {
        let sdr = Desc::new(1920, 1080, Format::R8G8B8A8Unorm).unwrap();
        assert_eq!(sdr.estimated_bytes, 1920 * 1080 * 4);
        let hdr = Desc::new(3840, 2160, Format::R16G16B16A16Float).unwrap();
        assert_eq!(hdr.estimated_bytes, 3840 * 2160 * 8);
        assert_eq!(Format::R10G10B10A2Unorm.dxgi_format(), 24);
    }

    #[test]
    fn rejects_zero_or_out_of_range_dimensions() {
        assert_eq!(Desc::new(0, 1080, Format::R8G8B8A8Unorm), Err(Error::ZeroExtent));
        assert_eq!(Desc::new(1920, 0, Format::R8G8B8A8Unorm), Err(Error::ZeroExtent));
        assert_eq!(Desc::new(16_385, 1080, Format::R8G8B8A8Unorm), Err(Error::ExtentTooLarge));
        assert_eq!(Desc::new(1920, 16_385, Format::R8G8B8A8Unorm), Err(Error::ExtentTooLarge));
    }

    #[test]
    fn output_size_estimate_is_exact_for_8k_formats() {
        let rgba8 = Desc::new(7680, 4320, Format::R8G8B8A8Unorm).unwrap();
        let rgba16f = Desc::new(7680, 4320, Format::R16G16B16A16Float).unwrap();
        assert_eq!(rgba8.estimated_bytes, 132_710_400);
        assert_eq!(rgba16f.estimated_bytes, 265_420_800);
    }
}


#[cfg(test)]
mod dlss_output_allocation_policy_tests {
    use super::{
        DlssOutputAllocationAction as Action, DlssOutputAllocationError as Error,
        DlssOutputAllocationPlan as Plan, DlssOutputFormat as Format,
        DlssOutputTargetDesc as Desc,
    };

    fn target() -> Desc {
        Desc::new(1920, 1080, Format::R8G8B8A8Unorm).unwrap()
    }

    #[test]
    fn reuses_an_identical_target_without_needing_budget_information() {
        let desc = target();
        let plan = Plan::new(Some(desc), desc, None).unwrap();
        assert_eq!(plan.action, Action::ReuseExisting);
        assert_eq!(plan.estimated_required_bytes, 0);
    }

    #[test]
    fn requires_a_known_budget_before_new_allocation() {
        assert_eq!(
            Plan::new(None, target(), None),
            Err(Error::BudgetUnavailable)
        );
    }

    #[test]
    fn rejects_allocation_when_headroom_would_exceed_budget() {
        let desc = target();
        let required = desc.estimated_bytes * 5 / 4;
        assert_eq!(
            Plan::new(None, desc, Some(required - 1)),
            Err(Error::InsufficientBudget {
                required_bytes: required,
                available_bytes: required - 1,
            })
        );
    }

    #[test]
    fn allows_new_allocation_when_budget_covers_headroom() {
        let desc = target();
        let required = desc.estimated_bytes * 5 / 4;
        let plan = Plan::new(None, desc, Some(required)).unwrap();
        assert_eq!(plan.action, Action::AllocateNew);
        assert_eq!(plan.estimated_required_bytes, required);
    }

    #[test]
    fn reallocates_when_extent_or_format_changes() {
        let current = target();
        let requested = Desc::new(2560, 1440, Format::R16G16B16A16Float).unwrap();
        let required = requested.estimated_bytes * 5 / 4;
        let plan = Plan::new(Some(current), requested, Some(required)).unwrap();
        assert_eq!(plan.action, Action::AllocateNew);
        assert_eq!(plan.estimated_required_bytes, required);
    }
}


#[cfg(test)]
mod descriptor_layout_tests {
    use super::{DlssDescriptorSlot, DlssDescriptorTableLayout};

    #[test]
    fn descriptor_table_has_one_slot_per_required_resource() {
        let layout = DlssDescriptorTableLayout;
        assert_eq!(DlssDescriptorTableLayout::DESCRIPTOR_COUNT, 4);
        assert_eq!(layout.slot(DlssDescriptorSlot::SceneColor), 0);
        assert_eq!(layout.slot(DlssDescriptorSlot::Depth), 1);
        assert_eq!(layout.slot(DlssDescriptorSlot::MotionVectors), 2);
        assert_eq!(layout.slot(DlssDescriptorSlot::Output), 3);
    }

    #[test]
    fn descriptor_byte_offsets_use_device_increment() {
        let layout = DlssDescriptorTableLayout;
        assert_eq!(layout.byte_offset(DlssDescriptorSlot::SceneColor, 32), Some(0));
        assert_eq!(layout.byte_offset(DlssDescriptorSlot::Depth, 32), Some(32));
        assert_eq!(layout.byte_offset(DlssDescriptorSlot::MotionVectors, 32), Some(64));
        assert_eq!(layout.byte_offset(DlssDescriptorSlot::Output, 32), Some(96));
    }

    #[test]
    fn descriptor_byte_offsets_reject_zero_increment() {
        assert_eq!(
            DlssDescriptorTableLayout.byte_offset(DlssDescriptorSlot::Output, 0),
            None
        );
    }

    #[test]
    fn descriptor_layout_checks_slot_bounds() {
        let layout = DlssDescriptorTableLayout;
        assert!(layout.contains(0));
        assert!(layout.contains(3));
        assert!(!layout.contains(4));
        assert!(!layout.contains(u32::MAX));
    }

    #[test]
    fn descriptor_slots_are_unique_and_contiguous() {
        let layout = DlssDescriptorTableLayout;
        let slots = [
            layout.slot(DlssDescriptorSlot::SceneColor),
            layout.slot(DlssDescriptorSlot::Depth),
            layout.slot(DlssDescriptorSlot::MotionVectors),
            layout.slot(DlssDescriptorSlot::Output),
        ];
        let mut sorted = slots;
        sorted.sort_unstable();
        assert_eq!(sorted, [0, 1, 2, 3]);
    }
}

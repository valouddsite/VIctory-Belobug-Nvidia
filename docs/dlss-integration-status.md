# DLSS integration status

## What exists today

The Windows host renderer creates a Direct3D 12 device, a host direct queue, a window and a DXGI swap chain. Its current `Host::present_planes` path takes the completed guest output texture (and optionally a second Series S plane), copies or composites it into the swap-chain back buffer, and calls `Present`.

That is a presentation path, not a DLSS integration. At this boundary the renderer does not yet expose a verified temporal input set for the guest scene.

## Safe groundwork in this branch

`crates/victory-gpu/src/dlss.rs` adds:

- named DLSS quality presets and planning-only render-resolution scales;
- a `FrameInputs` contract requiring scene color, depth, motion vectors and sub-pixel jitter (exposure is optional for SDR);
- a CPU-only temporal metadata validator requiring motion-vector extent to match render extent, finite nonzero vector scales, finite pixel-space jitter, explicit vector units/direction and valid previous-frame history; it does not generate vectors, alter the projection matrix or provide history textures;
- a CPU-only temporal history lifecycle tracker that invalidates continuity on first frame, render-extent changes, camera cuts, explicit resets and non-consecutive frame indices; it does not allocate, clear or validate GPU history textures;
- a deterministic Halton(2,3) sub-pixel jitter sequence generator returning centered render-pixel offsets; projection-matrix application and matching Streamline constants remain unwired;
- a `TemporalFramePlan` that advances history tracking and returns the same frame's render extent, Halton jitter, history-validity flag and reset reason as one CPU-side value; it does not apply projection jitter or create GPU history;
- a unified `plan_dlss_evaluation` gate that validates required input availability, render/output extents, exact frame-jitter and history consistency, non-aliased color/depth/motion-vector resource identities, and completed-submission shader-read transition proposals; it is still CPU-only and issues no barriers or Streamline calls;
- a resolution-plan type with zero-size validation;
- an explicit `NotIntegrated` backend state;
- a one-time warning when `VICTORY_DLSS` is requested, while leaving the existing presentation path unchanged;
- a pure color/depth compatibility validator for 2D extent, sample count and typed DSV format;
- frame-scoped retention of compatible color/depth candidates, keyed by the exact guest color-resource identity and the upcoming present frame;
- consumption only when that exact color resource is presented with the matching host-present serial; ambiguous color targets paired with different depth resources are rejected, and retained host resources live through the presentation call;
- exact recording-list association: `draw::set_targets` records the host-list identity, `Host::submit` binds candidates on the submitted list to its monotonically increasing submission token, and presentation accepts the candidate only after `FINISHED` has reached that token (the host updates `FINISHED` after the queue fence wait); candidates on lists never submitted are rejected;
- a CPU-only shader-read transition planner that rejects null identities, mismatched recording lists, unsubmitted/incomplete submission tokens, unknown source states and non-shader-readable targets; it returns a barrier proposal only and never emits a D3D12 command;
- diagnostic sampling of the existing `HostTexture.state` values after candidate completion, with conservative distinction between shader-resource-readable states, other read-only states, render-target, depth-write, UAV and copy states; mixed write/read and unknown bits are rejected by the classifier. The shader-readability flags are diagnostics only and do not issue barriers or authorize evaluation;
- a conservative global quiescence gate that additionally discards a candidate if title submissions are still queued or any host GPU submission remains in flight;
- a generic frame-candidate registry with unit tests for exact identity, serial mismatch, conflicting pairs, sticky invalidation and stale-entry pruning.

Environment variables:

| Variable | Meaning at this stage |
|---|---|
| `VICTORY_DLSS=1` | Request DLSS; logs that it is unavailable and falls back to normal presentation |
| `VICTORY_DLSS_QUALITY=quality` | Store the future target preset name |
| `VICTORY_DLSS_QUALITY=balanced` | Store the future target preset name |
| `VICTORY_DLSS_QUALITY=performance` | Store the future target preset name |
| `VICTORY_DLSS_QUALITY=ultra-performance` | Store the future target preset name |

These variables do not activate upscaling. The resolution scales in the preflight module are planning values only and are not applied to guest rendering resources.

The unified evaluation preflight is intentionally stricter than the standalone metadata checks: it rejects a frame until all required inputs are declared, render extents and jitter match the same temporal frame, previous-frame history is valid, all three GPU resource identities are distinct, and the associated submission has completed. It returns transition proposals for color, depth and motion vectors, but those proposals are not recorded D3D12 barriers and do not prove cross-queue ownership. No evaluation can run from this contract alone.

## Remaining implementation gates

1. **Presentation/depth association:** the draw layer now validates the first bound color target against its DSV for matching 2D dimensions, sample count and a supported typed depth-view format. This is a binding-time diagnostic only: it does not prove that the pair belongs to the final presented plane, and it does not yet bridge that pair to `Host::present_planes`. The draw layer now retains compatible candidates by exact guest color-resource identity and upcoming present frame. At presentation, the candidate is consumed only if that exact color resource is the one being shown in the same frame; ambiguous pairs are discarded, and the host color/depth resources are retained through the presentation call. This establishes a conservative host-present-serial association and resource lifetime, but it is not yet a GPU submission/fence token and does not transition or bind depth for a DLSS evaluation. It must not be used as proof that the depth contents are synchronized with the presented color until command-list submission ordering and resource states are explicitly tracked. The candidate is now associated with the exact host command-list object and the submission token assigned when that list is sent. Presentation rejects unsubmitted candidates and requires that the token has completed (`FINISHED` is advanced only after the host fence wait). The global quiescence gate remains as an extra conservative restriction. A CPU-only transition planner now validates resource/list identity and host submission completion before describing a possible shader-read transition. It does not own or mutate resource state, does not submit barriers, and cannot prove that other queues have not changed the resource. Remaining work is to establish authoritative per-resource state ownership across every recording list and queue, connect that ownership to actual D3D12 transition/barrier recording, and prove that the final presented color and depth contents correspond to the same render pass. The newly added state snapshot reads the existing atomic tracker only after completion and classifies it for diagnostics; it is not a synchronization proof, does not account for all possible cross-queue transitions, and does not emit a barrier. The diagnostic classifier now distinguishes actual PIXEL/NON_PIXEL shader-resource read bits from other read-only states, and rejects write/read mixtures or unknown bits. The pair is still not exposed to an upscaler. This token is the host's ordered submission serial; it is not itself the numeric D3D12 fence value, although completion is published only after the associated fence wait.
2. **Temporal data:** identify whether the guest provides a usable motion-vector render target or whether the renderer can generate correct per-pixel vectors for all geometry; add sub-pixel projection jitter and preserve previous-frame transforms. The metadata contract checks extent, finite scales/jitter, explicit units/direction and previous-frame validity, but it does not verify vector accuracy or produce the actual inputs. A CPU-only history tracker now detects first frame, resize, camera cuts, explicit resets and frame-index gaps; it does not manage GPU history textures or previous transforms. A deterministic Halton(2,3) sequence generator now produces centered sub-pixel offsets in render-pixel units. `TemporalHistoryTracker::plan_frame` now packages the matching frame index, render extent, jitter sample and history reset/validity result into one CPU-side plan, with tests for first frame, consecutive frames, resize, camera cuts, explicit resets, frame discontinuities and zero extents. This avoids mismatching temporal metadata, but the renderer still does not apply the offset to the guest projection matrix or pass matching jitter constants to Streamline. Do not substitute guessed vectors or frame differencing.
3. **Render graph / ordering:** expose scene color before tone mapping and UI composition; run the upscaler at the correct point, then composite UI at output resolution.
4. **Resolution management:** separate guest internal render dimensions from output/swap-chain dimensions; handle resize, aspect ratio, dynamic resolution and history invalidation.
5. **Streamline backend:** select and pin a compatible NVIDIA Streamline SDK release; integrate the official headers/import libraries and runtime DLLs with signature verification and license/package review; initialize it at the required point in device startup; query adapter/feature support; tag the actual D3D12 resources and constants; evaluate DLSS SR; and clean up feature/device resources on shutdown.
6. **Fallback and UI:** distinguish requested from actually active state. If initialization, resource tagging, evaluation or support checks fail, log the reason and continue with the normal renderer. Never label DLSS active unless evaluation really ran.
7. **Validation:** add deterministic tests for candidate association, recording-list/submission identity, fence-completion gating, quiescence, input validation, resolution transitions, history resets and fallback; run Windows CI; then test on a physical supported RTX GPU and compare image quality, frame time and VRAM use against the same scene without upscaling.

## Why this is not enabled yet

NVIDIA's DLSS integration guidance requires accurate scene color, depth, motion vectors, compatible jitter and correct resource tagging. The existing final presentation texture alone does not provide those inputs. The project also does not currently link or initialize Streamline. Enabling an upscaler without these pieces could produce unstable output or corrupt the render path.

Official references:

- [NVIDIA Streamline SDK and integration checklist](https://developer.nvidia.com/rtx/streamline/get-started)
- [NVIDIA Streamline repository and programming guides](https://github.com/NVIDIA-RTX/Streamline)
- [NVIDIA DLSS programming guide](https://github.com/NVIDIA/DLSS)

## Verification boundary

A passing CI run can validate Rust code, unit tests and Windows compilation. It cannot certify DLSS operation on the user's RTX 5060. That requires the official runtime components, a complete renderer integration and a physical hardware test.

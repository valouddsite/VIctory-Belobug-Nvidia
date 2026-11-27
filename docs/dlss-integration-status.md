# DLSS integration status

## What exists today

The Windows host renderer creates a Direct3D 12 device, a host direct queue, a window and a DXGI swap chain. Its current `Host::present_planes` path takes the completed guest output texture (and optionally a second Series S plane), copies or composites it into the swap-chain back buffer, and calls `Present`.

That is a presentation path, not a DLSS integration. At this boundary the renderer does not yet expose a verified temporal input set for the guest scene.

## Safe groundwork in this branch

`crates/victory-gpu/src/dlss.rs` adds:

- named DLSS quality presets and planning-only render-resolution scales;
- a `FrameInputs` contract requiring scene color, depth, motion vectors and sub-pixel jitter (exposure is optional for SDR);
- a resolution-plan type with zero-size validation;
- an explicit `NotIntegrated` backend state;
- a one-time warning when `VICTORY_DLSS` is requested, while leaving the existing presentation path unchanged.

Environment variables:

| Variable | Meaning at this stage |
|---|---|
| `VICTORY_DLSS=1` | Request DLSS; logs that it is unavailable and falls back to normal presentation |
| `VICTORY_DLSS_QUALITY=quality` | Store the future target preset name |
| `VICTORY_DLSS_QUALITY=balanced` | Store the future target preset name |
| `VICTORY_DLSS_QUALITY=performance` | Store the future target preset name |
| `VICTORY_DLSS_QUALITY=ultra-performance` | Store the future target preset name |

These variables do not activate upscaling. The resolution scales in the preflight module are planning values only and are not applied to guest rendering resources.

## Remaining implementation gates

1. **Presentation/depth association:** bridge the final color texture passed to `Host::present_planes` to the matching depth resource recorded by `draw::State::set_targets`; validate matching dimensions, format, frame lifetime and resource state before exposing it to an upscaler. Existing DSV creation and binding support is useful groundwork, but is not yet a per-present DLSS input contract.
2. **Temporal data:** identify whether the guest provides a usable motion-vector render target or whether the renderer can generate correct per-pixel vectors for all geometry; add sub-pixel projection jitter and preserve previous-frame transforms. Do not substitute guessed vectors or frame differencing.
3. **Render graph / ordering:** expose scene color before tone mapping and UI composition; run the upscaler at the correct point, then composite UI at output resolution.
4. **Resolution management:** separate guest internal render dimensions from output/swap-chain dimensions; handle resize, aspect ratio, dynamic resolution and history invalidation.
5. **Streamline backend:** integrate a pinned, licensed NVIDIA Streamline SDK release, load only verified official binaries, initialize it at the required point in device startup, query feature/hardware support, tag resources and constants, evaluate DLSS SR, and clean up resources on shutdown.
6. **Fallback and UI:** distinguish requested from actually active state. If initialization, resource tagging, evaluation or support checks fail, log the reason and continue with the normal renderer. Never label DLSS active unless evaluation really ran.
7. **Validation:** add deterministic tests for input validation, resolution transitions, history resets and fallback; run Windows CI; then test on a physical supported RTX GPU and compare image quality, frame time and VRAM use against the same scene without upscaling.

## Why this is not enabled yet

NVIDIA's DLSS integration guidance requires accurate scene color, depth, motion vectors, compatible jitter and correct resource tagging. The existing final presentation texture alone does not provide those inputs. The project also does not currently link or initialize Streamline. Enabling an upscaler without these pieces could produce unstable output or corrupt the render path.

Official references:

- [NVIDIA Streamline SDK and integration checklist](https://developer.nvidia.com/rtx/streamline/get-started)
- [NVIDIA Streamline repository and programming guides](https://github.com/NVIDIA-RTX/Streamline)
- [NVIDIA DLSS programming guide](https://github.com/NVIDIA/DLSS)

## Verification boundary

A passing CI run can validate Rust code, unit tests and Windows compilation. It cannot certify DLSS operation on the user's RTX 5060. That requires the official runtime components, a complete renderer integration and a physical hardware test.

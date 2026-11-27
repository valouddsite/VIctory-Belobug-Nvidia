<div align="center">

<img src="assets/branding/xboxx-logo.svg" alt="Xboxx — NVIDIA RTX 5060" width="680" />

### NVIDIA RTX 5060 · WINDOWS x64 · DIRECT3D 12

**Xboxx — a community compatibility project focused on NVIDIA support, GPU diagnostics and lower CPU overhead.**

<img alt="Platform" src="https://img.shields.io/badge/PLATFORM-WINDOWS%2010%2F11-00C8FF?style=for-the-badge&logo=windows&logoColor=white" />
<img alt="Language" src="https://img.shields.io/badge/RUST-2021-DEA584?style=for-the-badge&logo=rust&logoColor=white" />
<img alt="Graphics" src="https://img.shields.io/badge/GRAPHICS-DIRECT3D%2012-9B5DE5?style=for-the-badge" />
<img alt="Target" src="https://img.shields.io/badge/TARGET-RTX%205060-76B900?style=for-the-badge&logo=nvidia&logoColor=white" />
<img alt="Status" src="https://img.shields.io/badge/STATUS-EXPERIMENTAL-FFB347?style=for-the-badge" />

[Quick start](#-quick-start) · [RTX 5060 status](#-rtx-5060-status) · [Optimisations](#-cpu--pipeline-optimisations) · [Testing](#-build--validation) · [Credits](#-credits)

</div>

---

> [!IMPORTANT]
> **RTX 5060 compatibility and playable performance are not yet demonstrated.**
> The project has automated Windows CI coverage, but CI does not run on a physical RTX 5060. No FPS, frame-time or VRAM improvement is claimed without measurements from real hardware.

## ✦ Project overview

Xboxx is based on [VIctory](https://github.com/Marvisak/VIctory), a compatibility layer for running an Xbox Series title on Windows from files supplied by the user.

This fork concentrates on:

- NVIDIA GeForce support and accurate adapter/capability diagnostics.
- Direct3D 12 and shader-path compatibility.
- Memory-conscious behaviour for GPUs with 8 GB of VRAM.
- Reducing avoidable CPU-side contention in pipeline loading and diagnostics.
- Reproducible Windows builds, automated regression tests and a simpler test launcher.

The work is experimental. It does not turn every Xbox title into a working Windows game, and this repository does not include game files or copyrighted game assets.

## ◈ RTX 5060 status

| Area | Current status |
|---|---|
| Host graphics API | Direct3D 12 |
| Adapter discovery | Generic DXGI selection with startup diagnostics |
| NVIDIA RDNA translation path | Explicitly disabled; AMD-only path remains gated |
| NVIDIA RTX 5060 runtime | Not yet validated on physical hardware |
| 8 GB VRAM policy | Regression-tested thresholds; not yet tuned from RTX 5060 measurements |
| Windows x64 build | Automated package workflow available |
| Real game performance | No RTX 5060 FPS/frame-time result published |

### Hardware evidence — keep the results separate

The inherited VIctory reference measurements were collected on an Intel Core i9-12900K, 64 GB RAM and a Radeon RX 7900 XTX (24 GB VRAM), with ray tracing disabled. They are **not RTX 5060 results**.

| Reference scene / configuration | Reported result |
|---|---:|
| City street, Series S profile, patches 1, 2, 5, 6 and 7 | ~44.9 ms/frame (~22 FPS) |
| Same scene, patches 1, 2, 5 and 6 | 45.6 ms/frame, p95 47 ms |
| Same scene, patches 1, 2 and 5 | 47.3 ms/frame, p95 49 ms |
| Series X profile, patches 1 and 2 | ~53 ms/frame (~19 FPS) |

An inherited RTX 3090 report showed partial execution at approximately 0.53–0.55 seconds per frame. That does not establish RTX 5060 compatibility.

## ✦ CPU & pipeline optimisations

Recent work targets CPU overhead around the host pipeline library and diagnostics:

| Component | Change |
|---|---|
| Pipeline load guards | 256 digest-selected lock stripes instead of 64 |
| Pipeline-call diagnostics | 64 thread-ID-sharded tracking locks instead of one global lock |
| Diagnostic call removal | `swap_remove` avoids shifting the remaining vector entries |
| Early-pipeline tracking | 64 digest-selected shards with an atomic global limit of 100,000 keys |
| Early-pipeline key storage | 128-bit BLAKE3 prefixes stored as `u128`, rather than individually allocated strings |
| Conversion cache publication | Concurrent writers cannot turn a valid cache hit into a Windows rename failure; the first complete entry is retained |
| Shader cache keys | The translator build fingerprint is hashed as a separate prefix, avoiding a full copy of each shader container before cache lookup |
| Texture upload scheduling | Pending ahead-of-list uploads are indexed by host-copy serial for constant-time lookup and removal instead of repeated linear scans |
| Upload worker batches | Each long-lived copy worker reuses its batch vector capacity instead of allocating and freeing a new vector for every submission |
| Upload arena allocation | Reserved blocks are marked under the allocator lock, then zeroed after releasing it so large uploads do not serialize unrelated arena allocations |
| Persistence | Existing 32-character hexadecimal `early-pipelines.txt` format retained |

These changes reduce specific sources of lock contention and allocation overhead. They do **not** prove higher FPS or lower frame times; those effects require controlled measurements on the target GPU.

## ◈ Quick start

### Option A — one-click Windows launcher

1. Download the latest available `VIctory-RTX5060-Windows-x64` artifact (the executable package keeps its original technical filename) from the [GitHub Actions runs](https://github.com/valouddsite/VIctory-Belobug-Nvidia/actions).
2. Extract the ZIP to a writable folder.
3. Double-click `VIctory-Launcher.cmd`.
4. Select your own game executable.
5. Choose the normal or safe profile.
6. Read the generated log in `logs/` if startup fails.

The launcher creates `victory.toml` from the example configuration when needed. The selected game directory is passed only to that launch session. PowerShell's temporary execution-policy bypass is scoped to the launcher process; it does not change the machine-wide or user-wide policy.

### Option B — manual launch

Requirements:

- Windows 10/11 x64.
- An AVX2-capable CPU.
- A Direct3D 12 GPU with Shader Model 6.6 and mesh-shader support.
- The pinned Rust toolchain and Visual Studio C++ build tools when building from source.
- Your own legally obtained game files.

Build and launch:

```powershell
git clone https://github.com/valouddsite/VIctory-Belobug-Nvidia
cd VIctory-Belobug-Nvidia
Copy-Item victory.example.toml victory.toml
# Edit victory.toml and set your own paths.
cargo build --locked --profile diag -p victory --features diagnostics
.scriptsplay.ps1 -Exe "D:\path\to\your\game.exe"
```

For advanced options, see [docs/rtx-5060-test-release.md](docs/rtx-5060-test-release.md).

## ⬡ Windows test package

The [Windows test package workflow](https://github.com/valouddsite/VIctory-Belobug-Nvidia/actions/workflows/windows-test-package.yml) builds a Windows x64 ZIP containing:

- `victory.exe`
- `victory.example.toml`
- `VIctory-Launcher.cmd`
- PowerShell launch scripts
- `TEST-ON-YOUR-PC.txt`

GitHub Actions artifacts are temporary and expire according to the retention period configured in the workflow. A successful CI build verifies compilation and automated tests only; it is not a GPU hardware certification.

## ◈ Renderer switches

The following experimental renderer patches are opt-in while they are being tested:

| Variable | Purpose |
|---|---|
| `VICTORY_PORT_CONSTANTS=1` | Read the title's constants heap directly on the GPU |
| `VICTORY_PORT_DESCRIPTORS=1` | Track descriptor-table writes rather than scanning the full table |
| `VICTORY_PORT_CBV=1` | Read translated-shader constant buffers in place |
| `VICTORY_PORT_GUARD=1` | Split write-guard work across 64 KiB blocks |
| `VICTORY_PORT_JOURNAL=1` | Shard heap journal records and reduce submission-thread waits |

Do not enable experimental switches blindly. Change one variable at a time and keep the logs and exact configuration for comparison.

## ⌘ Build & validation

The Windows CI pipeline checks:

- `cargo test --workspace --locked -- --test-threads=1`
- `cargo test --workspace --all-features --locked -- --test-threads=1`
- Clippy and diagnostics compilation
- Windows launcher syntax and package-entry validation

Run locally:

```powershell
cargo test --workspace --locked -- --test-threads=1
cargo test --workspace --all-features --locked -- --test-threads=1
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Test results should always be reported with the exact workflow run and commit. A test passing on GitHub's Windows runner must never be described as a successful test on an RTX 5060.

## ◈ Project notes

- [RTX 5060 test-release guide](docs/rtx-5060-test-release.md)
- [Findings and performance notes](docs/findings.md)
- [Ray-tracing notes](docs/fork-ray-tracing.md)
- [Asset policy](docs/asset-policy.md)

## ✦ Credits

This repository preserves and builds on work from other projects and contributors:

- [VIctory](https://github.com/Marvisak/VIctory) — original compatibility layer, loader, runtime services, Direct3D 12 backend and shader recompiler.
- [fxceless/victory-fork](https://github.com/fxceless/victory-fork) — upstream fork lineage and contributions.
- [DexNom/VIctory](https://github.com/DexNom/VIctory) — ray-tracing fixes, cube gradients and sparse-texture work.

## License

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE). The file excluded from that license is identified in [docs/asset-policy.md](docs/asset-policy.md).

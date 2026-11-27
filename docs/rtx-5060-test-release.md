# Windows RTX 5060 test package

This is an experimental package for a first local compatibility test. It is not a stable release and does not establish that the RTX 5060 can run a particular title.

## Build package

After this workflow has been merged into the repository's default branch:

1. Open **Actions** in the GitHub repository.
2. Select **Windows test package**.
3. Choose **Run workflow** and the branch containing the tested code.
4. Wait for the run to finish. Download the `VIctory-RTX5060-Windows-x64` artifact from the completed run.
5. Extract the ZIP on the Windows PC.

The workflow runs the default workspace tests, builds `victory.exe` in optimized release mode for Windows x64, and packages the executable, example configuration, launcher and this test guide. The artifact is retained for 14 days. In the ZIP, `victory.exe` is at the extracted folder root; the launcher automatically uses that packaged binary for `-Build release` (source checkouts continue to use `target\\release\\victory.exe`).

## Local setup

Requirements:
- Windows 10 or 11, x64.
- AVX2-capable CPU.
- Direct3D 12 GPU with Shader Model 6.6 and mesh-shader support.
- A current graphics driver.
- Your own legally obtained game files and executable.

1. Extract the ZIP somewhere writable, keeping your game files outside that folder.
2. Double-click `VIctory-Launcher.cmd`.
3. Select your game's executable in the file picker.
4. Choose the normal profile first. If startup or rendering is unstable, rerun and choose the safe profile.

The launcher creates `victory.toml` from the example if needed, passes the selected game folder only to that launch session, verifies the resolved configuration, and writes a log under `logs/`. The CMD entry point uses a temporary PowerShell execution-policy bypass for its child process only; it does not change the user or machine policy.

Advanced users can still run the PowerShell launcher manually:

```powershell
.\scripts\play.ps1 -Exe "D:\path\to\your\game.exe" -Build release
.\scripts\play.ps1 -Exe "D:\path\to\your\game.exe" -Build release -Safe
```

Do not run the launcher as Administrator. Back up saves before testing.

## What to report

Record:
- GPU name, driver version and Windows build.
- CPU and RAM.
- Game version/build and selected profile.
- Whether startup succeeds, whether the menu/world renders, and where a failure occurs.
- The complete log and reproduction steps, after removing personal paths or other private information.

Never upload game files, keys, credentials or personal data.

## Validation boundary

The package workflow proves that the Windows executable builds and the automated workspace tests pass on GitHub's Windows runner. It does not prove that the RTX 5060 renders the title correctly, that 8 GB VRAM is sufficient, or that a target frame rate is reached. Those require a test on the physical PC.

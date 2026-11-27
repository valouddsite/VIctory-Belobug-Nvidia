# Windows RTX 5060 test package

This is an experimental package for a first local compatibility test. It is not a stable release and does not establish that the RTX 5060 can run a particular title.

## Build package

After this workflow has been merged into the repository's default branch:

1. Open **Actions** in the GitHub repository.
2. Select **Windows test package**.
3. Choose **Run workflow** and the branch containing the tested code.
4. Wait for the run to finish. Download the `VIctory-RTX5060-Windows-x64` artifact from the completed run.
5. Extract the ZIP on the Windows PC.

The workflow runs the default workspace tests, builds `victory.exe` in optimized release mode for Windows x64, and packages the executable, example configuration, launcher and this test guide. The artifact is retained for 14 days.

## Local setup

Requirements:
- Windows 10 or 11, x64.
- AVX2-capable CPU.
- Direct3D 12 GPU with Shader Model 6.6 and mesh-shader support.
- A current graphics driver.
- Your own legally obtained game files and executable.

1. Copy `victory.example.toml` to `victory.toml`.
2. Edit `[paths].game_dir` to point to your game data directory. Keep game data outside the extracted package.
3. Open PowerShell in the extracted package directory.
4. Run the normal release profile:

   ```powershell
   .\scripts\play.ps1 -Exe "D:\path\to\your\game.exe" -Build release
   ```

5. If the title fails to start or rendering is unstable, retry with the conservative profile:

   ```powershell
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

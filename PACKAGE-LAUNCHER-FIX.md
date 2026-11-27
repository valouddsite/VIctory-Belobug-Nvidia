# Launcher package layout fix

The Windows test package mirrors Cargo's release output at `target/release/victory.exe`, because `scripts/play.ps1 -Build release` resolves the executable from that path relative to the extracted package. The root-level `victory.exe` is also retained for direct inspection. The packaging workflow checks both paths and the launcher/configuration files before creating the ZIP.

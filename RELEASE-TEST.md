# Xboxx launcher release test

This branch is used to build the experimental Windows x64 launcher package.

## Validation scope
- Run the workspace test suite.
- Build the optimized Windows executable.
- Assemble and validate the launcher package and its PowerShell scripts.
- Upload the ZIP as a temporary GitHub Actions artifact.

## Important
This is a test package, not a validated RTX 5060 performance release. Physical GPU compatibility, stability, VRAM use and frame rate still require testing on the target PC.

The package does not include game files, keys or proprietary assets. Use only game files you are authorized to test.

# Building Argos (Windows)

Argos is a Windows-only screen-sharing app. Three of its Windows components pull in
**native C / assembly code** that must be compiled at build time:

| Crate | Why it needs a native toolchain |
| --- | --- |
| `openh264` / `openh264-sys2` | Compiles the openh264 C encoder from source (`cc` + `nasm`) |
| `webrtc` / `rtc-*` → `ring` | Crypto crate that builds x86-64 assembly (`nasm`) |
| `xcap`, `wasapi`, `winapi` | Windows APIs (screen capture, audio polling) |

**Consequence: even `cargo check` requires a working C compiler, a linker, and NASM.**
Rust alone is not enough on a fresh Windows machine. First-time builds are also slow
(a few minutes) because of the large `webrtc`/`egui` dependency tree — this is normal.

You need **either** recipe below. Recipe A (MSVC) is recommended; recipe B (GNU) is the
fallback. Builds, tests, and clippy have been verified with both.

## Prerequisites

1. **Rust** — install rustup from <https://win.rustup.rs/x86_64>, keeping the default
   stable toolchain.
2. **One native toolchain** — Recipe A **or** Recipe B.

## Recipe A — MSVC (recommended)

1. **Visual Studio Build Tools 2022** (provides `cl.exe`, `link.exe`, and the Windows SDK).
   Download the bootstrapper from <https://aka.ms/vs/17/release/vs_buildtools.exe> and run:

   ```
   vs_buildtools.exe --quiet --wait --norestart --nocache --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended
   ```

   (If `winget` is available: `winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`)

2. **NASM** (x86 assembler for `ring` / `openh264`). Install from <https://www.nasm.us>
   and make sure `nasm.exe` is on your `PATH`.

3. Build with the default toolchain:

   ```
   cargo build
   ```

## Recipe B — GNU (fallback)

1. **Install MSYS2** from <https://github.com/msys2/msys2-installer/releases>, e.g.:

   ```
   msys2-x86_64-latest.exe /S --root C:\msys64
   ```

2. **Install the mingw-w64 toolchain and NASM** (gcc, binutils, nasm):

   ```
   C:\msys64\usr\bin\bash.exe -lc "pacman -Sy --noconfirm --needed mingw-w64-x86_64-toolchain mingw-w64-x86_64-nasm"
   ```

3. **Install the GNU Rust toolchain:**

   ```
   rustup toolchain install stable-x86_64-pc-windows-gnu
   ```

4. Add the mingw tools to `PATH` for every build shell, then build with the GNU toolchain:

   ```
   $env:PATH = "C:\msys64\mingw64\bin;" + $env:PATH
   cargo +stable-x86_64-pc-windows-gnu build
   ```

## Verify the build

Run these from the repo root (verified green):

```
cargo check --workspace
cargo test -p argos-core --lib
cargo test -p argos-media --lib
cargo clippy --workspace --all-targets
```

Formatting:

```
cargo fmt --all -- --check
```

> Note: `cargo fmt --all` currently reports a pre-existing diff in
> `crates/argos-viewer` (an empty, unreferenced crate). It is unrelated to the
> app; if it bothers you, format only the active crates:
> `cargo fmt -p argos-core -p argos-media -p argos-app`.

## Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `linker 'link.exe' not found` | No Visual Studio Build Tools (MSVC path) | Recipe A, step 1 |
| `error calling dlltool 'dlltool.exe': program not found` | mingw binutils missing (GNU path) | Recipe B, steps 1–2 |
| `nasm` not found / assembly errors | NASM not installed or not on `PATH` | Recipe A step 2 / Recipe B step 2 |
| Build "fails" but ends with `Finished` | PowerShell renders cargo stderr as red `NativeCommandError` noise; `2>&1` pipelines also mess up exit codes | Ignore the red text and check the final line / `$LASTEXITCODE` |

## Scope

This document only covers getting a working build. The viewer crate
(`argos-viewer`) is empty and unreferenced — it can be ignored.
# Building Argos (Windows)

Argos is Windows-only. Two dependencies compile native code at build time, so a C
compiler and a linker are needed — but nothing else:

| Crate | What it needs |
| --- | --- |
| `openh264` / `openh264-sys2` | Compiles the openh264 C encoder from source (`cc`) |
| `webrtc` / `rtc-*` → `ring` | On MSVC, links pregenerated assembly — no assembler |

**NASM is not required on MSVC.** `ring` 0.17.14 ships pregenerated x86-64 assembly
for MSVC and only shells out to `nasm` on other toolchains. Verified by deleting
both crates and rebuilding from source on a machine with no `nasm` on `PATH`.

The GNU toolchain below is a different story — `ring` assembles its `.S` sources
there, so that path does want NASM. Only the MSVC recipe has been verified here.

Verified with `rustc 1.98.1` / `cargo 1.98.1`, MSVC toolchain, from a completely
empty `target` directory.

## Prerequisites

1. **Rust** — install rustup from <https://win.rustup.rs/x86_64>, keeping the
   default stable toolchain.
2. **Visual Studio Build Tools** (`cl.exe`, `link.exe`, and the Windows SDK).

   ```
   winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
   ```

   Or from the bootstrapper at <https://aka.ms/vs/17/release/vs_buildtools.exe>:

   ```
   vs_buildtools.exe --quiet --wait --norestart --nocache --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended
   ```

3. Build:

   ```
   cargo build
   ```

Cargo finds the MSVC toolchain on its own; no environment setup is needed for a
normal build from a fresh shell.

## The release build

The binary you actually run is the release one — the debug build is roughly ten
times slower to encode, which will make the stream look broken in a way it is not.

```
cargo build --release
```

It lands at:

```
target\release\argos-app.exe
```

To get a copy next to the source (this path is in `.gitignore`, and 13 MB of
executable does not belong in the history):

```
copy target\release\argos-app.exe argos-app-release.exe
```

A full rebuild from nothing — 9 GB of build cache, all of `ring`, `openh264`,
`webrtc` and `egui` — takes a few minutes. That is normal. Incremental builds
after that are seconds.

### Making sure the binary really is the new one

`cargo build --release` is incremental, so it will happily skip work and leave you
running the previous binary. If you are not sure whether what you are about to
run includes a change:

```
cargo clean -p argos-app -p argos-core -p argos-media   # drop just our crates
cargo build --release
Get-Item target\release\argos-app.exe | Select-Object LastWriteTime, Length
```

`LastWriteTime` is the honest check. If the source changed, it moves; if nothing
changed, cargo prints `Finished` in under a second and the timestamp stays put,
which is correct and not a failure.

For a build that cannot be stale in any way, `cargo clean` on its own throws away
everything:

```
cargo clean
cargo build --release
```

To compare two binaries rather than trust a timestamp:

```
Get-FileHash target\release\argos-app.exe -Algorithm SHA256
```

## Fallback — GNU toolchain

Not verified on this machine. Use it only if MSVC is unavailable.

1. **MSYS2** from <https://github.com/msys2/msys2-installer/releases>:

   ```
   msys2-x86_64-latest.exe /S --root C:\msys64
   ```

2. **mingw-w64 plus NASM** (this path does need the assembler):

   ```
   C:\msys64\usr\bin\bash.exe -lc "pacman -Sy --noconfirm --needed mingw-w64-x86_64-toolchain mingw-w64-x86_64-nasm"
   ```

3. **The GNU Rust toolchain:**

   ```
   rustup toolchain install stable-x86_64-pc-windows-gnu
   ```

4. Put the mingw tools on `PATH` for every build shell, then build with it:

   ```
   $env:PATH = "C:\msys64\mingw64\bin;" + $env:PATH
   cargo +stable-x86_64-pc-windows-gnu build --release
   ```

## Verify the build

All of these pass on the current tree:

```
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
cargo test -p argos-core --lib
cargo test -p argos-media --lib
cargo test -p argos-app
```

There is no `tests/` directory — every test lives in a `#[cfg(test)] mod tests`
next to the code it covers, so `-p <crate> --lib` finds them.

One media test is ignored by default because it needs an interactive desktop and
a GPU. To run it:

```
cargo test -p argos-media -- --ignored
```

## Layout

```
crates/argos-core    peer connections, RTP packetizer, link-quality maths, LAN signalling
crates/argos-media   Desktop Duplication capture, OpenH264 encode, WASAPI audio
crates/argos-app     the interface (egui)
```

## Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `linker 'link.exe' not found` | Visual Studio Build Tools missing | Prerequisites, step 2 |
| `error calling dlltool 'dlltool.exe'` | mingw binutils missing | Fallback, steps 1–2 |
| Double-clicking the exe does nothing | Not a fault — the app links as a GUI subsystem binary so it opens no console. It reports failures in the interface, and a panic leaves nothing on screen at all | Run `cargo run --release` to see panic output |
| Stream stutters badly on a fast machine | You are running the debug build | `cargo build --release` |
| Build "fails" but ends with `Finished` | PowerShell renders cargo's stderr as red `NativeCommandError` noise, and `2>&1` pipelines lose the exit code | Ignore the red text; check the last line or `$LASTEXITCODE` |
| The exe looks like the old one | Incremental build skipped the work | "Making sure the binary really is the new one", above |
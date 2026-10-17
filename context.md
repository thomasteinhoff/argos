# Argos — Context for Next Session

**Date:** 2026-10-17 · **Branch:** `main` · **Head:** `03c44db` (pushed)
**Working tree:** clean. Local-only: `context.md`, `todo.md` (gitignored) and the
`.gitignore` lines that ignore them.
**History:** the per-session log that used to fill this file was cut today. Every
section cited its own commit, so `git log` / `git show` has it — this file now
holds only what git cannot reconstruct.

---

## Status and the current goal

The app is feature-complete and usable: cursor overlay, explicit stream-end
notice, embedded icon, custom title bar, theme tokens and a real type scale all
shipped and verified on real machines.

**The goal changed on 2026-10-17.** It is no longer "maximise fps" but:

> **Stability and compatibility — Windows 10 and weak machines must not have
> headaches.**

The FPS backlog is folded into that goal under **Backlog**, and the two are not
in conflict: the encoding work below is what makes both a weak PC and a high
framerate work.

### What Argos Is

Windows-only Rust/egui screen-sharing app. **Share** (encode screen → H.264 →
WebRTC) and **View** (receive → decode → render). DTLS-SRTP over UDP. Discovery
is unauthenticated UDP broadcast on port **45892** (2 s beacon).

**Security model:** Radmin VPN is a closed, trusted network. No auth/consent
prompts — `on_view_request` auto-serves any peer. Both machines must be the same
build (H.264 `profile-level-id=42e01f` hardcoded). Personal use only.

---

## Current State

| Metric | Value |
|--------|-------|
| Tests | **130 passed / 0 failed / 2 ignored** (app 23, core 75, media 34) |
| Clippy | 0 warnings workspace-wide, `cargo fmt --all` clean |
| Release binary | rebuilt this session from `03c44db` → `argos-app-release.exe` (13.74 MB), SHA256 `4CFB7032A731A76006AE959145342F24D5BD9B60AE8A5DB902CDCCE9BF0DA8C5` |
| Last commits | `03c44db` robustness batch · `54bee67` the capture pacing fix · `ed46852` design pass #2 |

**`54bee67` — the pacing fix (the big one).** The 30 fps cap at every resolution
was the gate, not the cost: `last` was stamped after the readback while `due`
compared `last.elapsed()`, so on a high-refresh display most presents were
refused and nothing downstream could see it. Replaced with `due_at` — a fixed
deadline grid judged at the instant the acquire returned, which also carries the
remainder so a target averages exactly itself whatever the display does. Three
tests: a 110 Hz grid must average 30 fps (the old rule gave 27), an early
present must not drag the grid, a stalled grid must not burst.

**`03c44db` — robustness.** Encode queue 4→12, capture queue 1→2, `UP_HOLD`
8s→4s, `UP_DROPS` 1→3, bitrates 6M→5M / 4M→3M. Trade stability for latency (see
Open decisions).

---

## Locked decisions (2026-10-17)

1. **Hardware encoding is next, on by default with automatic fallback** to
   openh264, and no toggle to forget about. **Bitrates stay as they are** until a
   real stream shows text quality suffering.
2. **The friend's network case is deferred** until asked for — see Open
   questions for how it gets diagnosed.
3. **Parked:** a genuine Win10 run, the three presets, GPU zero-copy, the fps
   axis.

---

## Open questions

### The friend: 5 fps, in both directions

Win11, machine similar to the sharer's, and he fails as viewer *and* as sharer
while nobody else does. That pattern points at **the link, not the machines** —
one bad peer costs the same in either role. Diagnosis costs nothing: one look at
his row in the viewer panel (Ctrl+D).

| Reading | Goes to |
|---|---|
| `loss%` high | the transport. There is no NACK/RTX: a lost fragment kills the frame, a lost P-frame poisons its references, and recovery waits out the 4 s `KEYFRAME_INTERVAL`. Discord survives the same link because WebRTC retransmits. |
| `loss%` low, `send ms` high | the Radmin path to him specifically |
| `decode ms` high | his machine after all |
| `drops%` climbing | the sharer's encoder; the ladder is already reacting |

### Windows 10 is untested

**Nothing has ever run on genuine Windows 10** — this dev box reports "Windows
10" but is build 26200 (actual Win11). The capture's own failure path
(`D3D11CreateDevice` / `DuplicateOutput`) returns a plain `Err(String)` and what
a user sees on a weak GPU or under RDP has never been observed.

---

## Backlog

### 1. Hardware encoding (next, agreed)

- **2a — instrument only.** `MFTEnumEx` on the encode worker thread at startup;
  a new `encoder` row in the pipeline readout — `hardware (QuickSync)` /
  `hardware (NVENC)` / `software (openh264)` plus the fallback reason. No
  behaviour change. **Pick the encoder on the same adapter as the capture**:
  an Optimus laptop duplicates on the iGPU, so the other card would mean a
  cross-adapter copy.
- **2b — the MFT path**, behind the existing `H264Encoder` seam so `send_frame`,
  the packetizer, the decoder and the UI never learn which encoder ran.
  Constraints that must not be negotiated: **base profile, no B-frames** (the SDP
  hardcodes `42e01f`), `MF_TRANSFORM_ASYNC_UNLOCK` for async MFTs, COM and
  `MFStartup` on the encode worker thread, **output media type before input**.
  Fallback is exactly where 2a found it: silent, logged, reported.
- **Check to leave behind:** a test asserting the media type is
  `eAVEncH264VProfile_Base` with no B-frames, and that no encoder means openh264.
- **Needs:** `Win32_Media_MediaFoundation` + `Win32_System_Com` on the existing
  `windows 0.62`. No new dependency, single exe preserved.
- **Reference:** Chromium
  `media/gpu/windows/media_foundation_video_encode_accelerator_win.cc` —
  `MFT_MESSAGE_SET_D3D_MANAGER` with a `ID3D11VideoProcessor`, described by its
  own authors as "much better than readback".

### 2. Cheap, open

- **Presets:** Quality 1080p30 / Performance 720p60 / Max 1080p60. Max stays
  disabled until 2b. A preset is just `{height ceiling, fps}`; disabled while
  streaming like the existing combos, because the encoder's rate control is fixed
  at construction.
- **openh264 `num_threads`** is auto (0) today. The bench's `Camera t4` / `t8`
  1080p rows were never read; WebRTC's own heuristic (1080p + >8 cores → 8,
  >1280x960 + ≥6 cores → 3, else 1) says auto may be leaving 2x on a table.
- **`ColorImage::from_rgba_premultiplied`** in the viewer frame path — keep
  `unmultiplied` for the icon and cursor, which have real alpha.

### 3. Later

Bitrate scaled per rung (re-decide against `03c44db`'s 5M/3M first) · fps axis on
`quality.rs` · GPU-side scale/convert, zero-copy · transport NACK/RTX (the
friend's case) · `DuplicateOutput` failure messaging.

---

## Load-Bearing Code (Do Not "Simplify")

| Location | Why |
|---|---|
| `quality.rs` | `smoothed AND raw` double-gate before any ladder move; EMA zeroing each step; `MIN_HOLD` floor |
| `dxgi.rs` `due_at` | The cadence is a fixed deadline grid **on purpose**. Measuring from the last send, or after the readback, pinned the stream to 26-30 fps at every resolution — both measured, `54bee67` |
| `ui.rs` `worst_link` | The sharer adapts to the worst viewer, not the average |
| `diagnose.rs` | Loss-first ordering; `frame_budget_ms` returning `None` |
| `ReportSink` trait | Do not touch |
| `encode.rs` `new_at` | `CameraVideoRealTime` on bench data. One-line revert to `ScreenContentRealTime` if text looks blocky; costs ~4x encode speed (`b440196`) |

---

## Known Deferred (User-Approved)

`profile-level-id` hardcode on both peers · `scene_change_detect` stays false ·
A/V drift ~0.7 s/hour · 10 s UI freeze on a LAN send failure (`lan.rs:323`) ·
no further refactoring of `ui.rs` / `audio.rs` planned.

---

## Harness Gotchas

| Issue | Workaround |
|---|---|
| `Copy-Item` preserves mtime → cargo reuses a stale binary | after restore: `(Get-Item $p).LastWriteTime = Get-Date` |
| PowerShell renders cargo stderr as red `NativeCommandError` | trust `$LASTEXITCODE` |
| App is GUI subsystem — panics print nowhere | `cargo run --release` to see them |
| Tests are inline `#[cfg(test)] mod tests` only, no `tests/` dirs | clippy `items_after_test_module` fires if that module is not last in the file |
| `argos-media` DXGI test needs a GPU and is `#[ignore]`d | capture changes are read-verified unless a pure predicate is extracted — `due_at` exists because of this |
| Cargo output is noisy in PowerShell | pipe through `Select-String` |

---

## Needs a Real Run (Cannot Be Checked From Here)

1. Degrade a link → the diagnosis should say something, then recover to OK.
2. Share with Discord closed → the audio notice should appear.
3. Reset button → should zero the audio stats too.
4. Text sharpness on a real stream after the `CameraVideoRealTime` flip.

---

## Quick Commands

```bash
# Full gate
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Release build + copy + hash
cargo build --release -p argos-app
Copy-Item target\release\argos-app.exe argos-app-release.exe -Force
Get-FileHash argos-app-release.exe -Algorithm SHA256

# The only measured source of encoder costs
cargo test -p argos-media --release -- --ignored --nocapture encoder_preset_bench
```

---

## If You Pick This Up

Read **`todo.md`** first — it is the ordered list and the thing to tick. In one
line: instrument the encoder MFTs (2a), run it on the machines that exist, then
build the MFT path (2b), and only then the friend's case, which starts by
reading his viewer row rather than writing any code.

# Argos

Screen sharing over your own VPN. One person presses **Go Live**; anyone else on
the same network sees them appear in the list and clicks their name to watch.
No accounts, no servers, no cloud — the two machines talk to each other
directly.

Windows only, and deliberately so: capture goes through the Desktop Duplication
API, which is Windows-specific and has no equivalent elsewhere.

## Requirements

- Windows, practically 10 or later. Capture uses the Desktop Duplication API,
  which has existed since Windows 8, but nothing older has been tried.
- A network both machines already share. The intended one is
  [Radmin VPN](https://www.radmin.com/radmin-vpn/), which gives each machine a
  stable private IP and hides the peers from everyone else — see
  [Security](#security) for why that matters.
- A build toolchain, if you're building from source rather than taking a
  prebuilt binary. See [BUILDING.md](BUILDING.md).

Argos can launch Radmin VPN for you from the sidebar if it is installed.

## Using it

**To share.** Press **Go Live**. Pick a monitor. Anyone who connects appears in
the audience list, and you can see how long each person's picture has been
taking to write and whether their link is dropping packets.

**To watch.** Click a name in the peer list. The picture opens; **F11** goes
fullscreen, and the controls fade out of the way when you stop moving the
mouse. **Esc** leaves fullscreen.

**Shortcut keys**

| Key | Does |
| --- | --- |
| `F11` | Fullscreen while watching |
| `Esc` | Leave fullscreen |
| `F` | Fullscreen in the preview |
| `Ctrl+D` | Pipeline readout — see [Diagnostics](#diagnostics) |

**If someone doesn't appear in the list.** Peer discovery is a UDP broadcast,
which some networks do not carry between machines. Both sides have an
**Advanced** panel for that: the sharer presses **Create manual connection
code** and sends the resulting code to the viewer by any means you like — chat,
email, read it aloud. The viewer pastes it into **Connection code** and presses
**Connect**, then sends their own answer code back the same way. It is a
base64 blob of the WebRTC offer, so it is long and it is not meant to be typed
by hand.

That path also exists for adding another viewer to a share that is already
running, which is what **New code for another viewer** is for.

## Diagnostics

`Ctrl+D` opens a floating window with the numbers behind the picture: how long
capture, encode, packetize and write are taking (mean and peak), how many
packets are being dropped and where, and the same for the audio workers
including clock drift. It stays on top deliberately, because it is most useful
exactly when the video is stalling and the sidebar is not what you are looking
at.

There is also a **Copy** button that puts the whole readout on the clipboard,
which is the useful thing to paste into a bug report.

A **Diagnosis** line appears in two places, and it is the same measurement seen
from opposite ends. The viewer sees `Your end:` in the Watching panel — what
their own machine is experiencing. The sharer sees a diagnosis beside each name
in the audience list — what that person's link is doing. Both are one sentence
saying what is wrong and where, and both are recomputed continuously: if one
names something, that is what is happening right now, not something that
happened an hour ago.

## How it works

Three crates, roughly in dependency order:

| Crate | What lives there |
| --- | --- |
| `argos-core` | Peer connections, the H.264/Opus RTP packetizer, link-quality maths, and the LAN signalling channel |
| `argos-media` | Everything touching Windows: Desktop Duplication capture, OpenH264 encoding, WASAPI audio capture and playback |
| `argos-app` | The interface |

Two channels run between the machines:

- **Signalling** is a small JSON protocol over UDP on port 45892. It finds
  peers (everyone broadcasts a beacon every two seconds), carries the WebRTC
  offers and answers, and carries each viewer's own measurement of the link so
  the sharer can react to it.
- **Media** is ordinary WebRTC — H.264 video and Opus audio — negotiated over
  that signalling channel, then sent peer to peer.

The reason for two channels is that WebRTC cannot report its own quality. The
sharer cannot see what a viewer is actually receiving; only the viewer can
measure it. So viewers send back their own loss and frame rate over the
signalling channel every 500 ms, and the sharer uses that to walk the
resolution up or down. That feedback loop is what "Adjust resolution to the
viewer's link" drives, and it is why the checkbox is on by default.

Screen content gets its own encoder rate-control profile (`ScreenContentRealTime`)
rather than the camera one, because a desktop is mostly flat areas and sharp
text and does not want bits spent the way a camera image does.

## Security

This is the part worth reading before you use it on anything you care about.

**There is no authentication and no access control.** Peer discovery is an
unauthenticated UDP broadcast, and **a sharer serves any peer that asks, with
no approval prompt** — if you are live, someone who finds you in the list gets
your screen when they click your name. The only thing standing between an
outsider and your desktop is the network itself.

**Media is encrypted in transit.** WebRTC's DTLS-SRTP is mandatory in the
transport, so a stream is not readable to other machines on the same network.
That protects the video, not the decision to send it.

So the security model is exactly this: *the network is the trust boundary.* Use
it on a network you control, where everyone on it is someone you would hand a
remote-desktop session to. Radmin VPN fits well because it is a private
overlay: machines on it can find each other, and machines off it cannot see
any of them. A shared office WiFi does not.

Argos is built for a small group of people sharing over a VPN they already
trust. It is not hardened against an adversary on the same network, and it has
not been audited.

## Tests

109 tests, all passing:

```
cargo test -p argos-core --lib     # 72
cargo test -p argos-media --lib    # 23, plus 1 ignored
cargo test -p argos-app            # 14
```

One media test is ignored because it needs an interactive desktop and a GPU; it
is a capture round-trip and there is no way to fake that in CI.

Tests live in `#[cfg(test)] mod tests` next to the code they cover rather than
in a `tests/` directory, so they are read alongside the thing they test.

The packetizer has the most coverage, deliberately. Every frame passes through
it, and the FU-A fragmentation path — which a 1080p keyframe always takes, being
tens of kilobytes against a 1200-byte MTU — had none at all, because the one
test that existed used a 6-byte NAL that never fragments.

## Known limitations

- Windows only. Not portable, not intended to be.
- H.264 is fixed at baseline profile (`42e01f`). Both peers hardcode it, so they
  must be the same build.
- No chat, no file transfer, no input forwarding. It is a screen and the sound
  that goes with it.
- Audio capture is system loopback. If Discord is running, Argos excludes it —
  the sidebar tells you which of the two is happening.
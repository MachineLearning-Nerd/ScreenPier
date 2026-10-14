# Rust-owned WebRTC session

## Usage

You open a private HTTPS URL in Comet on your Mac. You authenticate, connect, and control the existing Linux desktop. Linux browser tests and evidence remain available to Codex there. Audio and agent integration are outside this candidate.

These conceptual call sites describe the host caller. They are not implementation.

```rust
let host = Host::open(policy)?;
let connection = host.connect(authenticated_grant, requested_session)?;
host.stop()?;
```

The browser owns fullscreen and image scaling. The host validates coordinates against the negotiated output geometry. A second tab requests takeover instead of obtaining another controller.

## Problem

[Requirements](../requirements.md) demand readable text, smooth UI motion, remote unlock, and bounded resource use on the physical Hyprland session. [Evidence](../evidence.md) establishes available protocols, but proves neither capture nor encoding. Hyprland reports full-frame damage, so dirty rectangles cannot drive the design.

## Shape

Model the Domain puts controller ownership and display health in one session state machine. The proposed types are conceptual. Signatures have no implemented bodies.

```rust
struct ControllerLease { id: SessionId, epoch: Epoch }
enum DisplayHealth { AwaitingFrame, Current(FrameId), Stale, Unavailable }
enum DesktopState { Unlocked, Locked, Unknown }
enum SessionState { Disconnected, Negotiating, Connected(ControllerLease) }
enum Input { Move(Point), Button(ButtonAtPoint), Key(KeyTransition), Scroll(ScrollAtPoint) }

impl Host {
	fn open(policy: HostPolicy) -> Result<Self, HostError>;
	fn connect(&self, grant: AuthenticatedGrant, request: SessionRequest)
		-> Result<Connection, ConnectError>;
	fn stop(&self) -> Result<(), HostError>;
}
```

The small host interface hides negotiation, cleanup, and media recovery. `AuthenticatedGrant` has a private constructor. `SessionRequest` contains output and quality preferences. HTTP and media adapters keep signaling types private and parse wire messages into domain inputs.

- `session` owns the controller lease, pressed inputs, display health, and clipboard permissions. One actor accepts commands. Disconnect, takeover, and stop release held inputs and invalidate the epoch.
- `desktop` owns Wayland capture, output geometry, XKB mapping, virtual input, and text clipboard. It translates Mac keys explicitly. Lock or unknown lock state clears clipboard requests. Unlock uses the real lock screen and password entry.
- `media` owns str0m, UDP sockets, timers, and the native encoder adapter. It drains str0m output after each mutation. Bitrate estimates and keyframe requests update the encoder.
- `http` serves the browser and authenticates signaling. HTTPS uses a separate Serve endpoint and preserves the existing port 443 root mapping.

The proposed authentication boundary trusts Serve identity headers only through its loopback proxy. It restricts access to the configured owner. A short-lived session capability binds the signaling exchange to its DTLS fingerprint and controller epoch. UDP reachability alone grants no desktop access. Keys, passwords, and clipboard text never enter diagnostics.

Reliable ordered control messages carry key transitions and button events. A separate channel coalesces absolute pointer movement. Button and scroll events include their own coordinates. Reconnect discards pending input. Decoded-frame acknowledgments and a deadman timeout suspend input on stale video.

## Stream policy and tradeoffs

H.264 is the first codec candidate. Hardware encoding requires validation. Software encoding has a CPU budget. The raw-frame queue holds the latest frame. Drop obsolete raw frames before encoding, never arbitrary dependent encoded frames.

Compositor damage is unavailable. Start with codec prediction and bounded capture cadence. Validate application image comparison before adopting it. Congestion reduces cadence and bitrate first. Resolution reduction exposes the quality tradeoff to the user. Recovery requests a fresh keyframe. This policy promises no bandwidth or latency number.

We accept codec text artifacts and encoder integration work in exchange for browser video decoding and WebRTC congestion feedback. One controller and one output keep resource limits explicit.

## Alternatives and synthesis

GStreamer `webrtcsink` hides more media plumbing but adds dependencies and separates transport ownership from Rust session policy. Rectangle streaming can preserve exact text but needs its own motion, congestion, and recovery policy. Both remain synthesis alternatives. This candidate supplies no final selection.

## Risks and staged validation

Can Comet negotiate and decode the encoder output? Can capture and input operate through Hyprland lock? Can Tailscale route the separate UDP media port through direct and relayed paths?

After execution authorization, validate capture and remote unlock first. Then validate encoder interoperability, mapped input, and clipboard. Finally measure realistic scrolling and animation under constrained networks, disconnects, monitor changes, and compositor restarts. Reject this candidate if those gates fail.

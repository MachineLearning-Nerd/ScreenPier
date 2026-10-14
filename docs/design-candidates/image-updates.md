# Desktop image updates over WebSocket

## Usage

You open the private HTTPS endpoint in Comet, authenticate, and control the existing Linux desktop. You test apps in the Linux browser. Codex and Claude use evidence there. Fullscreen changes presentation and pointer mapping. Reconnection restores the current desktop.

Conceptual caller examples follow. These signatures are proposals, not implementations.

```rust
let host = DesktopHost::attach(existing_output, owner_policy)?;
let controller = host.connect(authenticated_owner)?;
controller.apply(Input::Pointer(pointer_in_output_space))?;
controller.exchange_clipboard(ClipboardAction::Paste(text))?;
```

## Problem

[Requirements](../requirements.md) require readable text, smooth animation, clipboard, and remote unlock. [Evidence](../evidence.md) establishes advertised protocols, not working capture or input. Hyprland reports full-frame damage. This candidate therefore compares captured pixels itself. It uses HTTPS WebSocket and canvas instead of a video pipeline.

## Shape

Model the Domain puts controller ownership and framebuffer dependencies into explicit types.

```rust
struct DesktopHost;
struct ControllerLease;
struct FrameId { epoch: Epoch, sequence: Sequence }
enum DisplayUpdate {
	Snapshot { id: FrameId, pixels: Image },
	Patch { base: FrameId, id: FrameId, tiles: Vec<Tile> },
}
enum SessionState { Syncing, Ready(FrameId), Stale, Closed }
enum Input { Key(KeyTransition), Pointer(OutputPoint), Button(ButtonTransition), Scroll(ScrollDelta) }
impl DesktopHost {
	fn attach(output: OutputId, policy: OwnerPolicy) -> Result<Self>;
	fn connect(&self, owner: AuthenticatedOwner) -> Result<ControllerLease>;
}
impl ControllerLease {
	fn apply(&self, input: Input) -> Result<()>;
	fn exchange_clipboard(&self, action: ClipboardAction) -> Result<()>;
}
```

`desktop` owns Wayland capture, input, clipboard, and output recovery. `session` owns authentication capabilities, the controller lease, pressed keys, and acknowledged framebuffer state. `images` owns comparison, compression, and bounded update scheduling. `web` owns HTTPS adapters and the canvas client. Wire messages remain private. The public methods hide sequencing and cleanup.

The proposal grants one controller. A second tab requests takeover. Disconnect, lease expiry, and takeover release remote held input. A local stop revokes the lease. Capture exists only while needed. Output changes increment the epoch and invalidate coordinate mappings.

An owner credential establishes a secure HttpOnly session cookie. WebSocket admission verifies origin and session ownership. Tailscale restrictions add a network boundary. A separately selected HTTPS endpoint preserves the existing port 443 service. Clipboard actions require explicit gestures, bounded UTF-8, and no content logs.

## Update policy

Begin with independently compressed lossless PNG tiles. Compare pixel blocks despite compositor damage reports. Send nothing for unchanged pixels. The canvas stages a complete patch before committing and acknowledging its frame ID. Reject patches whose base differs from the displayed frame. Send a new snapshot after reconnect or epoch change.

Keep one outstanding update and one newest pending capture. Build the next patch against the acknowledged image. Discard obsolete pending work, never arbitrary transmitted dependencies. Bound bytes, decode work, and send time. Close stalled connections and resynchronize rather than accumulate delay. TCP retransmission delays outgoing image and control messages. Input uses the opposite direction. Acknowledgments also constrain update cadence by round-trip time.

Proposed scroll reuse copies regions from the acknowledged image only after exact pixel verification. Hashes find candidates but do not prove equality. Start without reuse. Add it only if comparison cost and bandwidth evidence justify it.

Keep native output dimensions for text. Map browser coordinates through the displayed rectangle and output transform. If motion exceeds capacity, reduce update frequency before resolution. Separate cursor delivery requires its own compatibility gate.

## Tradeoffs and alternatives

Lossless tiles preserve text at the cost of CPU comparison and bandwidth during animation. Smooth motion remains an acceptance risk. Full-frame reads may burden this four-core host.

WebRTC video hides congestion and decode behind browser media facilities. It is the stronger alternative if tiles fail motion or latency targets. It adds encoder and media-route integration. WayVNC with noVNC supplies mature rectangle handling but loses Rust ownership of the host. Neither alternative belongs in this candidate's first transport.

## Open gates and next step

Can capture and normal virtual input display and unlock the actual lock screen? Clear pending clipboard and old displayed content on lock. Authenticate remotely before forwarding the normal unlock password. Never bypass the locker or log input or credentials.

After execution authorization, validate capture and input first. Then compare tile streaming against the shared network workload in actual Comet. Test clipboard, scaling, stale display control suspension, direct and relayed paths, takeover, output changes, and recovery. No performance result is claimed. Parent synthesis selects or rejects this candidate.

# ScreenPier architecture

The provisional design is a Rust host with WebRTC desktop video and browser control in Comet. It targets smooth browser testing on the existing Linux desktop. Image updates remain a comparison candidate for exact text and sparse changes. Runtime evidence decides the production transport.

## User workflow

The user keeps Linux awake and signed in. On the Mac, the user connects Tailscale and opens the private app URL in Comet. The app displays the current physical desktop. Clicking, typing, scrolling, and dragging operate Linux directly.

The user tests apps in the Linux browser. Those sessions and any evidence the user saves stay on Linux. Codex and Claude operate through their existing tools. ScreenPier handles remote desktop access.

## Candidate comparison

| Candidate | Strength for this workflow | Cost or uncertainty |
| --- | --- | --- |
| Rust session with WebRTC video | Browser video decoding, congestion feedback, and a plausible route to smooth animation. | Encoder integration, separate media connectivity, text artifacts, and display-progress correlation need validation. |
| Rust image updates over WebSocket | Lossless text and application control over unchanged-region suppression. | CPU comparison, PNG bandwidth during motion, TCP blocking, and update acknowledgment cadence can limit responsiveness. |

The [WebRTC sketch](design-candidates/webrtc.md) is the provisional base. The [image-update sketch](design-candidates/image-updates.md) contributes explicit framebuffer generations and recovery semantics. This selection determines the first experiment, not a proved performance winner.

The first video experiment uses H.264 with low-delay encoder settings. Hardware encoding is preferred only after it works on the active GPU. A software encoder establishes correctness and fallback cost. Comet's actual offer and answer determine supported profiles and resolutions.

The image sketch allows one outstanding update before acknowledgment. At 80 ms round-trip delay, that imposes a theoretical ceiling of 12.5 updates per second before processing. At 100 ms, the ceiling is 10. It cannot satisfy the proposed 24 and 15 changing-frame targets as drawn. Bounded pipelining or independent updates would require a revised comparison sketch. These are derived limits, not benchmark results.

str0m is the initial Rust protocol-engine candidate. Its documented bitrate and keyframe feedback fit a Rust-owned session. GStreamer webrtcsink remains an integration alternative if it produces a simpler validated pipeline. Native encoder ownership must not leak into the browser or session API. [Protocol-engine responsibilities](https://github.com/algesten/str0m), [GStreamer integration](https://gstreamer.freedesktop.org/documentation/rswebrtc/webrtcsink.html).

## Component ownership

| Component | Owned knowledge and state |
| --- | --- |
| Desktop | Wayland connection, output resources, capture buffers, XKB map, virtual input, clipboard access, and observed lock state. |
| Session | Authenticated controller lease, input ordering, held keys and buttons, output generation, display progress, and clipboard permissions. |
| Media | Encoder lifetime, capture cadence, transport feedback, native buffer import, packet scheduling, and decoder recovery. |
| Web | HTTPS request parsing, owner authentication, browser client, signaling, and conversion of wire messages into domain commands. |

Session has one owner. Other components report events or accept commands. They do not independently maintain another list of held inputs or another controller identity.

The browser owns presentation geometry and fullscreen. The host owns actual output geometry. A geometry generation binds normalized browser coordinates to the output the user is viewing.

## Domain model

The conceptual shape precedes implementation.

- `OwnerGrant` represents an authenticated owner. Only the authentication boundary constructs it.
- `ControllerLease` contains a session identity, controller epoch, and expiry. Takeover invalidates the previous epoch.
- `OutputGeneration` changes when output identity or geometry changes.
- `DesktopGeometry` contains output identity, pixel size, scale, transform, and generation.
- `DisplayProgress` distinguishes a healthy static desktop from newly captured content that the client has not presented.
- `InputCommand` represents absolute movement, a key transition, a button transition with coordinates, or scrolling with coordinates.
- `PressedInputs` belongs exclusively to Session and drives release on revocation.
- `ClipboardAction` carries bounded text and an explicit direction within the current lease.
- `StreamBudget` contains validated bitrate, cadence, and queue bounds. Transport feedback updates it through one media policy.

The session states are Disconnected, Authenticating, Negotiating, AwaitingDisplay, Active, Degraded, Reconnecting, and Revoked. Lock state is separate from connection state because the active controller may need to unlock Linux.

Unknown lock state disables clipboard exchange. Display-unavailable or stale states suspend remote input and release held remote input. Reconnection establishes a new lease and synchronizes current geometry and display. It never replays old input.

## Image and input flow

Desktop supplies a captured image and geometry to Media. Media encodes current content and sends video through the accepted connection. The browser presents video and reports meaningful progress. Session admits control only for the current lease and a usable displayed generation.

Reliable ordered delivery carries key and button transitions. Pointer movement can replace older unsent absolute positions. Button and scroll events include their own position to avoid cross-channel ordering assumptions. Repeated keys and text composition require explicit mapping and tests on the Mac keyboard.

Capture buffers and queues have explicit ownership and limits. The raw-capture queue retains current work rather than accumulating old frames. Already encoded dependent video frames cannot be discarded arbitrarily. Decoder recovery forces an encoder keyframe through the encoder adapter.

The proposed presentation contract correlates the sender's RTP timestamp with Comet's `requestVideoFrameCallback` metadata. Media records a bounded mapping from transport epoch and RTP timestamp to captured frame identity and output generation. The browser acknowledges only frames submitted for composition. Session rejects acknowledgments outside the current mapping, lease, or generation.

RTP wraparound is resolved within the current transport epoch and bounded history. Each reconnect, observed lock transition, or output-generation change clears the old video element and establishes a new peer connection. Control waits for a matched frame from that new epoch. Old packets and callbacks cannot reactivate control. This accepts reconnection cost in exchange for an explicit reset boundary.

The browser API documents WebRTC RTP metadata as optionally available. Its actual availability and timestamp mapping on Comet are early validation gates. If absent or ambiguous, reject this correlation mechanism and redesign before permitting remote input. Generic decoder callbacks, heartbeat messages, and arrival counters are insufficient. [Video frame callback documentation](https://developer.mozilla.org/en-US/docs/Web/API/HTMLVideoElement/requestVideoFrameCallback).

Display freshness compares acknowledged presentation with the latest changed captured content. A repeated or unchanged image does not become stale merely because no new content appeared. Deliberately delayed old video after geometry changes, lock, and reconnect must never enable current control.

## Bandwidth policy

Begin with codec prediction and a bounded capture cadence. Hyprland reports full-frame damage, so application image comparison needs its own cost measurement before adoption.

Use transport bandwidth estimates to adjust the encoder budget. Leave headroom for input, retransmission, and protocol overhead. Reduce cadence before resolution when doing so preserves usable motion and text. The controller can reduce quality when content exceeds the available budget. Recovery increases quality gradually rather than filling queues with a burst.

Sparse text and moving UI have different encoding costs. Lossless image updates are the comparison for sparse content. If video fails readable-text targets, revisit the chosen transport or codec settings before adding a hybrid with two renderers.

## Private connection and authentication

An illustrative endpoint is `https://your-host.your-tailnet.ts.net:8443`. Check the port before configuration. Serve can proxy a separate loopback HTTP listener. Keep any existing port 443 route unchanged.

The HTTP boundary accepts only the configured owner. Serve identity headers are trusted only through the local proxy listener. The application checks origins and binds a short-lived capability to the negotiated session. WebRTC authentication binds that session to its expected remote transport identity and controller epoch. Reachability alone grants no control.

Owner identity is supplied locally before startup. An absent owner configuration refuses service. Remote requests cannot enroll an owner. The loopback listener accepts the expected Serve host and origin. Authenticated session cookies use Secure, HttpOnly, and SameSite restrictions. State-changing requests require an origin check and anti-CSRF protection. The execution recipe must verify these boundaries through the actual proxy.

Process-local signing state invalidates capabilities on restart. Stop, owner-policy replacement, and takeover revoke affected leases and release input. The transport's verified peer identity must match the accepted signaling capability before capture or input admission. No interactive approval on Linux is required for each owner connection.

Video and input over WebRTC require a separate media listener and ICE reachability through the tailnet. Prefer binding to the Tailscale interface. Do not introduce public STUN or TURN as a hidden fallback. If the direct media route fails, record the cause and evaluate the alternative private path.

One tab controls at a time. Another authenticated tab can request explicit takeover. A local stop command revokes the lease and cleans up input. No raw keys, passwords, clipboard bodies, or session capabilities belong in diagnostic logs.

## Lock and lifecycle behavior

Remote unlock uses normal lock-screen input. Capture must stop exposing prior desktop content when Linux locks. A lock transition clears queued clipboard work and pending user input. It does not bypass the locker or change its authentication.

Locked reconnect and unlock are first-version requirements. Hyprland's `locked` request is the initial authoritative state source. A local `hyprctl -j locked` query currently returned false. The v0.56.2 handler reads `isSessionLocked` from the session-lock manager. This proves an unlocked query works, not that locked-session capture works. [Hyprland request implementation](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/debug/HyprCtl.cpp#L1824).

The proposed monitor queries that IPC state during an active session and before clipboard actions. Use short-lived request sockets with bounded deadlines and always close them. Query errors, malformed responses, or missed deadlines produce Unknown and disable clipboard. Observed changes invalidate display state and pending input. Poll cadence and transition latency must be measured. Polling cannot promise an instantaneous lock boundary. Clipboard tests include transitions during an exchange.

Remote unlock remains an early local feasibility gate and a later end-to-end Mac gate. Protocol support alone cannot satisfy either.

Output changes invalidate old geometry. Capture resources are recreated as constraints change. Compositor restart requires a new Wayland connection. Suspend interrupts access and resume requires resynchronization. Remote wake and access before graphical login remain outside scope.

## Synthesis and unresolved gates

The base follows the motion-oriented WebRTC sketch. The image sketch contributes acknowledged display generations, resynchronization, and exact-text comparison workloads. Scroll reuse and automatic transport switching are deferred.

Independent review must check authentication, ownership, display freshness, and the comparison method. The runtime gates are capture correctness, active-GPU encoding, Comet presentation, Tailscale media reachability, input mapping, text clipboard, remote unlock, and the budgets in [validation](validation.md).

The next implementation step, after the user requests execution, is a local capture correctness check. No source code or runtime prototype accompanies this design.

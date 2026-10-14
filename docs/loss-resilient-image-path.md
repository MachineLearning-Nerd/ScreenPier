# Loss-resilient image path: prototype contract

Status: design for an isolated experiment. No image path is implemented or deployed.

## Why this experiment

At 0.5 Mbps, 200 ms RTT, and 2% random packet loss, the 1280 × 720 Focus H.264 candidate completed four of five guarded local runs, while whole-output Low data completed one of five. Focus read 12–16 px native text exactly in its completed runs. It still had 11–42 display-expiry falls per run, and one run rejected every constrained-phase input because the displayed frame was 546 ms old. Proxy queue drops were zero in all ten runs. A two-second H.264 keyframe interval made one guarded run worse. See [the exact measurements](viewport-prototype.md).

The measured problem is the age of the image the browser can actually display, especially after packet loss. Increasing the 500 ms changed-content input deadline would admit clicks against stale content. Lowering the bitrate alone already failed a separate test. The next experiment should change how sparse desktop updates reach the browser, not relax input authority.

The [RFB protocol](https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst) provides a useful principle: request the newest framebuffer state when the client is ready, and skip transient states on a slow link. This is an architectural inspiration, not evidence that a WebSocket implementation will beat the current video path. Chromium's [H.264 NACK/ULPFEC test](https://chromium.googlesource.com/external/webrtc/%2B/HEAD/video/video_send_stream_tests.cc) also cautions against assuming a simple FEC toggle is a fix; its test disables ULPFEC for H.264 with NACK as inefficient.

## Proposed mode

Keep Smooth motion on the current WebRTC H.264 path. Prototype a separately selected **Text view** using the same live Hyprland output and the same host-owned viewport. This mode captures no second desktop and creates no virtual session. It compares native RGBA pixels against the last browser-acknowledged image, sends only changed rectangles, and displays them on a canvas at one source pixel per CSS pixel. The first comparison uses lossless PNG rectangles because literal small-text clarity is the gate; a cheaper encoding can replace PNG only after decoded-pixel and CPU measurements.

Use a separate authenticated same-origin WebSocket for image bytes, keeping the existing reliable WebRTC data channel for input. This separation avoids putting large image messages ahead of key results on the control channel. The WebSocket endpoint must enforce the existing owner, origin, lease, generation, and takeover rules. Verify Tailscale Serve forwards the upgrade on the private route before building the media path. The `axum` WebSocket feature is available in its [official API](https://docs.rs/axum/latest/axum/extract/ws/), but route behavior is a local integration gate.

The first prototype is **demand-driven**: one committed update and at most one newest pending capture per connection. A browser ACK means all rectangles for a numbered update decoded and drew atomically to an offscreen canvas, then became visible. The next diff is based on that ACKed image, never on an unacknowledged capture. If its changed rectangles exceed a byte or processing budget, discard obsolete captures and send a fresh bounded snapshot rather than accumulating a queue. Full snapshots are required after reconnect, pan, lock transition, output change, or any missed base. A partially decoded patch is never visible or authoritative.

Every update carries session epoch, output generation, viewport sequence and rectangle, base update ID, new update ID, content revision, and capture identity. The host validates bounds and dependencies. The browser rejects mismatched bases or dimensions and requests a fresh snapshot. The host binds input to the **displayed, ACKed update ID** and uses its capture time for the existing 500 ms changed-content and 750 ms capture-health limits. A mode switch revokes held input, clears old display proof, and waits for an ACK of the new mode. Reuse the current explicit key/button outcome IDs; never replay uncertain input.

An image update may be late under TCP retransmission. That must show as a paused control state, not as a grant of extra image lifetime. A one-outstanding-update policy also limits motion rate at high RTT. Text view is intended for sparse browser and editor changes; Smooth remains the animation mode. The client may choose modes, but automatic switching is deferred until each mode passes independently.

## Measurement sequence

1. Build an offline diff/PNG benchmark from exact 1280 × 720 physical fixture captures: first snapshot, typing, scrolling, pointer motion, modal opening, and animation. Record encoded bytes, CPU time, changed area, and decoded pixel equality. Reject the approach if ordinary typing cannot fit the 0.5 Mbps budget with headroom.
2. Implement the endpoint and renderer behind an opt-in flag in an isolated process. Test owner/origin/lease admission, bounded messages and queues, atomic canvas commit, stale base rejection, pan/reconnect/lock, and input mapping. Do not make an image ACK equivalent to a video RTP ACK without an explicit domain type.
3. Run the same five alternating physical-desktop route tests at 0.5 Mbps / 200 ms RTT / 2% loss. Require five of five exact native input runs, 14 px OCR in every phase, current display within five seconds, bounded memory and queue time, and no diagnostic failure. Count every readiness fall and gap; compare bytes and host CPU with H.264 Focus.
4. Validate the accepted artifact on the actual Mac Comet tailnet route for text, panning, shortcuts, lock/unlock, recovery, and a sustained browser-testing workload. Keep the installed service unchanged until the implementation and Mac result are reviewable.

The first benchmark can reject this design before any WebSocket or browser renderer work. A failed image path should remain isolated; it is not a reason to deploy the current Focus candidate as if it met the weak-link target.

## First offline feasibility receipt

A guarded disposable Chromium fixture produced five 941 × 943 RGB screenshots: an initial page, a text-field change, and three sparse animation samples. The capture wrapper verified the fixture window, kept the private controller idle, and restored the original physical workspace. The ignored receipts are `.audit/receipts/image-path-fixture/manifest.json` and `tile-benchmark.json`; the repeatable scripts are in `.audit/screenpier-image-benchmark/`.

An offline Python comparison decoded the screenshots to RGBA, compared equal-size pixel tiles exactly, and estimated per-tile PNG transfer bytes using filter-0 scanlines and zlib level 1. This is an early data-size screen, not the proposed Rust implementation or a network test:

| Tile edge | Text change | Sparse animation changes | Largest single tile |
| --- | ---: | ---: | ---: |
| 64 px | 5.4 KB across 7 tiles | 3.9–5.1 KB across 13–15 tiles | 1.2 KB |
| 128 px | 9.8 KB across 5 tiles | 5.0–6.0 KB across 6–8 tiles | 3.8 KB |
| 256 px | 12.8 KB across 3 tiles | 21.0–27.2 KB across 4–5 tiles | 8.1 KB |

At a theoretical 0.5 Mbps payload rate, 5.4 KB takes about 86 ms to transmit before protocol overhead, RTT, browser decode, and tailnet effects. The 64 px tiles are promising for this sparse fixture, but 941 × 943 is smaller and simpler than the real Focus crop and contains no video. The benchmark does not measure host capture cost, production CPU, WebSocket loss recovery, or perceived latency. The physical-output and route tests above remain mandatory.

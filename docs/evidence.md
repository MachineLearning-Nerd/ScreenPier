# ScreenPier technical evidence

## Local observations

Read-only inspection on 13 October 2026 found an empty project directory and no applicable ancestor AGENTS.md files.

| Component | Observation | Scope of evidence |
| --- | --- | --- |
| Desktop | Hyprland 0.56.2 in the existing Wayland session. | Version query only. |
| Display | eDP-1, 1920 by 1080, scale 1. | Active monitor query. |
| CPU | Intel i5-7300HQ with four cores. | Hardware inventory. |
| Graphics | Intel HD Graphics 630 and NVIDIA GTX 1050 Mobile. | PCI inventory, not encoding proof. |
| Rust | rustc and cargo 1.98.1 are installed. | Version queries. |
| Capture | WLR screencopy v3 and ext image capture v1 protocols are advertised. | Wayland registry inspection, not a captured frame. |
| Input | Virtual keyboard v1 and virtual pointer v2 are advertised. | Registry inspection, not an input test. |
| Clipboard | WLR and ext data control protocols are advertised. | Registry inspection. |
| Permissions | ecosystem:enforce_permissions returned false. | Current setting only. Do not alter it. |
| Lock state | hyprctl -j locked returned false. | Unlocked query only. The v0.56.2 handler reads the session-lock manager. |
| Encoder adapters | FFmpeg lists software and hardware H.264 adapters. GStreamer resolves nvh264enc but not vah264enc. | Installed adapters, not successful encoder operation. |
| Dependencies | intel-media-driver and libva are installed. Some GStreamer plugin packages and vainfo are absent. | Package inspection. |
| Network | Tailscale Serve on port 443 proxies the root to 127.0.0.1:3000. | Existing service status. Preserve this mapping. |

No capture, remote input, encoder benchmark, installation, service exposure, or desktop configuration change has occurred during preparation.

## Capture limitations from source

Hyprland v0.56.2's image capture and shared frame implementations report full-frame damage. Useful compositor dirty rectangles cannot be assumed. Application-level image comparison or codec prediction must establish bandwidth savings. [Image capture](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/protocols/ImageCopyCapture.cpp), [screen frame implementation](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/managers/screenshare/ScreenshareFrame.cpp).

SHM capture reads pixels into shared memory. DMA-BUF capture avoids that particular readback, but an encoder must accept the device, format, modifiers, and synchronization. Import across Intel and NVIDIA devices requires testing. Separate cursor capture also has permission and monitor-commit dependencies. [Screen frame implementation](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/managers/screenshare/ScreenshareFrame.cpp), [capture protocol implementation](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/protocols/ImageCopyCapture.cpp).

Virtual keyboard input requires an XKB keymap. Browser key events, Mac modifiers, Linux keycodes, and pressed-input cleanup need an application policy. Virtual pointer coordinates must account for output transforms and display scaling. [Keyboard implementation](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/protocols/VirtualKeyboard.cpp), [pointer implementation](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/protocols/VirtualPointer.cpp).

Monitor destruction stops capture. Mode changes can change buffer constraints. Lock, monitor power-off, suspend, and compositor restart need runtime recovery tests. [Capture session](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/managers/screenshare/ScreenshareSession.cpp).

## Streaming alternatives from documentation

str0m is a Rust WebRTC protocol engine. The application supplies encoded frames, socket traffic, timers, and signaling. Bitrate estimates and keyframe requests must drive the encoder. Its documented run loop requires draining output after each mutation. [str0m](https://github.com/algesten/str0m), [events](https://docs.rs/str0m/latest/str0m/enum.Event.html).

Rust GStreamer webrtcsink is another integration option with congestion control and bitrate adaptation. Its larger dependency stack may replace some application media plumbing. [webrtcsink](https://gstreamer.freedesktop.org/documentation/rswebrtc/webrtcsink.html).

WebRTC's baseline codecs include H.264 and VP8. Its standard warns that 4:2:0 video representation can be unsuitable for screen content. Text readability must be evaluated alongside resolution and motion. [RFC 7742](https://www.rfc-editor.org/rfc/rfc7742.html).

Image rectangles can retain exact text and avoid retransmitting unchanged areas. RFB CopyRect can reuse pixels already in the client framebuffer, including scrolling. Our capture source does not supply that reuse automatically. [RFC 6143](https://www.rfc-editor.org/rfc/rfc6143.html).

WayVNC attaches to a running Wayland session. noVNC needs a WebSocket connection and can use websockify to bridge VNC. These are a reference path for comparison, not the confirmed Rust application design. [WayVNC](https://github.com/any1/wayvnc), [noVNC](https://novnc.com/noVNC/).

## Browser and Tailscale boundaries

Perplexity documents Comet as Chromium-based. That establishes its engine family, not this Mac's negotiated codec, permissions, shortcut behavior, or decode performance. [Comet documentation](https://www.perplexity.ai/help-center/en/articles/12781449-comet-for-enterprise).

Browser video frame callbacks can report the RTP timestamp of WebRTC video submitted for composition. The documentation marks that metadata optional. Comet must establish the mapping before the host can trust frame-specific progress acknowledgments. [Frame callback documentation](https://developer.mozilla.org/en-US/docs/Web/API/HTMLVideoElement/requestVideoFrameCallback).

Serve can provide HTTPS page delivery and signaling. WebRTC additionally requires a media connection, typically UDP, reachable through the tailnet. An HTTPS proxy is not itself that media connection. Direct and relayed Tailscale paths need separate measurement. [Serve](https://tailscale.com/docs/features/tailscale-serve), [connection types](https://tailscale.com/docs/reference/connection-types).

## Evidence still required

The first technical experiments must establish capture correctness, working hardware and software encoding, browser negotiation, cursor behavior, input mapping, clipboard semantics, remote unlock, network reachability, and recovery. Bandwidth and latency claims require the same realistic workload, stated network conditions, and measurements on the actual Mac client.

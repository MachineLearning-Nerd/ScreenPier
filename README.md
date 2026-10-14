# ScreenPier

ScreenPier runs on this Linux machine. A browser on your Mac displays and controls the same physical Hyprland desktop. Applications and test evidence stay on Linux.

This is an early implementation. [Runtime results](docs/runtime.md) separate verified behavior from remaining acceptance checks.

## Build and check

Run these commands in the project directory.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
node tests/client-lifecycle.cjs
cargo build --release --bin screenpier
```

The host needs FFmpeg, libxkbcommon, wl-clipboard, Hyprland's screencopy and virtual input protocols, and Tailscale. Run the application inside the existing graphical login. The current hardware encoder uses the Intel render device at `/dev/dri/by-path/pci-0000:00:02.0-render`.

## Start private access

ScreenPier must run inside the signed-in Hyprland session. [The service example](deploy/screenpier.service.example) needs your owner login, Tailnet hostname, Tailscale address, output, and executable path before installation as `screenpier.service`. Check an installed service with `systemctl --user status screenpier.service`. Keep any other Tailscale Serve routes intact.

The commands below also support a manual host run when the user service is stopped.

Set these values to your Tailscale login, this machine's Tailscale address, and its HTTPS hostname.

```sh
export SCREENPIER_OWNER_LOGIN='your-tailscale-login@example.com'
export SCREENPIER_MEDIA_IP='100.64.0.1'
export SCREENPIER_ORIGIN='https://your-host.your-tailnet.ts.net:8443'
```

Start the host in a terminal on Linux.

```sh
./target/release/screenpier \
  --owner-login "$SCREENPIER_OWNER_LOGIN" \
  --public-origin "$SCREENPIER_ORIGIN" \
  --media-ip "$SCREENPIER_MEDIA_IP" \
  --output eDP-1 \
  --encoder vaapi \
  --bitrate-kbps 3000
```

If the hardware encoder is unavailable, start with `--encoder software`. The software encoder uses more CPU.

In a second terminal, add the private HTTPS proxy.

```sh
tailscale serve --bg --https=8443 --yes http://127.0.0.1:6080
tailscale serve status
```

If Serve reports access denied, run the Serve command with `sudo` and enter the password in your terminal. Removing the proxy may also require `sudo`.

If port 443 already serves another application, leave that route intact. Use port 8443 for this application. Tailscale Serve provides private HTTPS and user identity headers. [Serve documentation](https://tailscale.com/docs/features/tailscale-serve).

## Connect from your Mac

1. Connect the Mac to Tailscale using the configured owner's account.
2. Open your configured Tailscale HTTPS URL in Comet.
3. Click **Connect**. Wait for **Connected** and the current Linux screen.
4. Click the displayed desktop before typing.

The **Quality** menu starts in **Clear text** (800 kbps, 15 fps). Choose **Low data** (560 kbps, 8 fps) on a slower link or **Smooth motion** (up to 2400 kbps, 20 fps) when motion matters. Switching reconnects the video session and releases held keys. If Smooth falls behind, the app returns to Clear text. These are fixed profiles within the host's bitrate cap; link-speed detection is still experimental.

**Mac Command sends Ctrl** maps Command shortcuts to Linux Control shortcuts. Uncheck it to send Linux Super. Comet and macOS can reserve some shortcuts before the page sees them. Physical key mapping currently uses US layout. Exchange non-ASCII text through the clipboard buttons.

**Paste to Linux** copies your browser's text clipboard into Linux's clipboard. Then paste inside the Linux application. **Copy from Linux** copies Linux clipboard text into your Mac clipboard. Browser permission is required. Text is limited to 64 KiB, with an additional 72 KiB encoded-message limit. Clipboard exchange is disabled while locked or when lock state is unknown.

Only one tab controls the desktop. **Take control** replaces the current controller and releases its held keys and buttons. **Disconnect** releases remote input. A stale displayed image pauses control.

The computer must remain awake and signed into Hyprland. An authenticated production connection wakes the monitor through Omarchy. Enter your password only into the ordinary Linux lock screen. Remote unlock passed a Mac Comet check on an earlier binary with the same lock and input handling; this quality-mode build still needs a Mac check. Later graphical-session lifecycle cases remain open.

## Stop access

For an installed `screenpier.service`, run `systemctl --user stop screenpier.service`. For a manual host run, press Ctrl+C in its terminal. Remove only this application's proxy when needed.

```sh
tailscale serve --https=8443 off
tailscale serve status
```

Do not reset all Serve configuration. Other ports may belong to existing services.

## Diagnose a connection

The HTTP listener binds to `127.0.0.1:6080`. WebRTC media binds separately to the configured Tailscale address on UDP port 6081. Tailnet policy must permit both HTTPS 8443 and UDP 6081 between the Mac and Linux. No public STUN or TURN service is configured.

If admission is rejected, check the exact owner login and HTTPS origin. If video is unavailable, check the selected monitor, encoder, and private UDP route. The browser retries transient connection failures for up to one minute. Disconnect cancels recovery. Admission rejection and another controller owning the desktop require an explicit action. A sustained foreground video stall triggers bounded recovery even when control messages continue. If recovery stops, reconnect explicitly. The host status endpoint `/api/status` requires the same owner authentication.

Start with `--diagnostics-log ./screenpier.jsonl` to record numeric timing and input counters. Peer and readiness records use schema version 2 and include the outgoing video stream's sampled queue size and oldest unsent packet age; this queue also includes retransmissions and padding. `sample_age_ms` indicates how recent that snapshot is. The writer runs separately from transport and uses a bounded queue. It creates an owner-only file, rejects symbolic links, and truncates that configured file when it reaches 8 MiB. A logger failure does not interrupt desktop control. `/api/status` reports dropped records and logger failure.

Logs include capture, encoding, presentation age, transport scheduling, and input receipt, admission, expiry, cancellation, and backend completion counts. Backend completion does not prove the target application received an event. Duration fields ending in `last_ms` contain the latest sample. Scheduling fields ending in `max_ms` contain the session maximum. Desktop actor counters accumulate for the host lifetime. Logs exclude keys, clipboard contents, passwords, desktop images, controller credentials, and negotiation data.

Audio, IME composition, access before graphical login, and remote power-on are not included. The installed user service is enabled for `graphical-session.target`; its automatic start after a fresh graphical login still needs a real login test. Performance budgets are targets until measured on the actual Mac route.

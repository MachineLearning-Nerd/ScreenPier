# ScreenPier requirements

ScreenPier runs on the user's Linux machine. Comet on the user's Mac opens a private Tailscale URL and controls the same desktop shown on the Linux physical monitor.

## Confirmed workflow

The user opens apps and browser pages on Linux, tests them remotely, and leaves the browser session and testing evidence on Linux. Codex or Claude already running there can investigate that evidence using their available tools. ScreenPier provides desktop access.

The primary workload is everyday desktop use, browser app testing, Herdr, Codex, terminals, scrolling, and UI animation. Smooth motion and readable text both matter. Watching video is outside the intended workload.

## First version

- The host application uses Rust. Native codec libraries may provide encoding. The browser client may use JavaScript or TypeScript.
- The client is Comet on macOS with Tailscale connected.
- Capture attaches to the existing Hyprland session and physical output. Capture does not launch another desktop.
- Remote input controls the existing desktop. Local changes remain visible to the remote user.
- Keyboard, pointer, scrolling, dragging, and text clipboard exchange are included.
- Fullscreen and scaling preserve pointer alignment with the displayed image.
- The user can connect while away, see the Linux lock screen, and unlock it remotely. This capability requires a live Hyprland test.
- Disconnects release remote held keys and buttons. Reconnection restores the current desktop rather than replaying old input.
- The host remains awake and signed in to its graphical session.
- Existing Tailscale services remain intact. The current port 443 root proxies to local port 3000.
- Software installation, desktop configuration changes, service setup, and implementation start belong to the subsequent authorised execution phase.

## Proposed defaults

These defaults settle reversible design choices without extending the confirmed scope.

- One active remote controller. A second tab gets an explicit takeover choice. Takeover releases the previous controller's held input.
- Local use and remote use affect the same desktop. A local stop command terminates remote control without logging out.
- The first target is eDP-1 at 1920 by 1080. Disconnect or mode changes trigger resource recovery. A multi-monitor interface can follow.
- Desktop control and text clipboard are the initial feature set. Audio is deferred. Dedicated agent integration, file transfer, and automatic recording are deferred.
- Clipboard exchange uses explicit user actions and bounded UTF-8 text. Clipboard content is not written to diagnostic logs.
- Locking clears pending clipboard actions. Unlock passwords and keystrokes are not logged or recorded.
- Failed or stale video visibly pauses control. The client identifies reconnection and stale display states.
- CPU and queue limits prevent the remote app from making local development unusable.
- Start and stop operate within the user's graphical session. Automatic startup can be added after manual lifecycle validation.

## Access boundaries

Access is private to Tailscale. The browser uses HTTPS. Public internet publishing is outside scope.

The application authenticates the configured owner before it issues a session capability. Tailscale access alone does not authorise every tailnet user to control the desktop. The final handshake must protect both signaling and any media or input connection.

Remote unlock applies to an existing logged-in graphical session. Access before graphical login, disk decryption, remote power-on, and operating-system recovery are outside the first version.

## Acceptance status

Requirements are settled enough for design. Performance numbers in the validation plan are proposed targets, not measurements or guarantees. Codec selection, GPU encoding, Comet interoperability, remote unlock, and end-to-end reliability remain technical validation gates.

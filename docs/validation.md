# Validate desktop access and bandwidth use

Use the same live Linux desktop and the actual Mac Comet client for final acceptance. Protocol advertisements, installed encoder adapters, and Linux Chromium tests cannot substitute for that result.

All numbers below are proposed engineering targets. Diagnostic runs are recorded in [runtime results](runtime.md). No performance budget has passed final acceptance. Adjusting a target requires a recorded reason and user review when the change affects the intended experience.

## Establish the measurement method

1. Record the host commit, Rust and native library versions, graphical session, active GPU, output geometry, macOS version, Comet version, and negotiated codec.
2. Record host load and local browser responsiveness with the remote application stopped.
3. Create a deterministic browser test page on Linux with small coloured text, editable fields, scrolling, dragging, and UI animation.
4. Include normal work in Herdr and the Linux browser after the deterministic workload.
5. Record app payload bytes and actual network bytes separately. State whether Tailscale overhead and relay traffic are included.
6. Separate capture time, encoding time, queue time, browser presentation time, and input-to-visible-response time.
7. Use monotonic clocks within each device. Do not subtract unsynchronised Mac and Linux timestamps.
8. Measure input-to-visible-response on the Mac through an echo test page on Linux. Correlate unique actions with their resulting display changes.
9. Distinguish no screen changes from failed capture or stalled delivery. Heartbeats establish session health but do not prove that a changed image reached the client.
10. Validate correctness and count failures inside every timed run. Do not count a rejected input or frozen animation as a fast response.

Measure end-to-end behavior in addition to individual stages. If a result is near its target, identify the limiting CPU, GPU, network, encoding, or client component in a separate profiling run.

## Compare the streaming candidates

Use release builds and production encoder settings. For each condition, perform five runs per candidate and alternate their order. Each run contains 30 seconds of idle desktop, 60 seconds of typing, 60 seconds of scrolling, and 60 seconds of animation.

Keep output geometry, content, client display size, and correctness expectations the same. Warm both paths before timed runs and record startup separately. Report medians, ranges, percentiles, and failed runs. A difference smaller than run variation is inconclusive.

Throttle only isolated test traffic. Do not alter the machine's global network settings or interrupt other Tailscale services. Any test setup that needs desktop, firewall, or system changes gets a separately reviewed recipe.

| Profile | Payload budget | Added round-trip delay | Packet loss | Intended decision |
| --- | --- | --- | --- | --- |
| Baseline | No artificial cap. | None. | None. | Find capture, encoder, and client limits. |
| Desktop | 5 Mbps. | 80 ms. | 0.5 percent. | Evaluate smooth UI motion. |
| Limited | 2 Mbps. | 100 ms. | 1 percent. | Evaluate everyday desktop usability. |
| Slow | 1 Mbps. | 150 ms. | 1 percent. | Evaluate readable text and dependable typing. |
| Very slow | 0.5 Mbps. | 200 ms. | 2 percent. | Evaluate graceful degradation and recovery. |

Measure both the actual Mac route and the isolated profiles. Record direct versus relayed Tailscale connectivity. Do not present synthetic traffic shaping as a measurement of the user's internet connection.

## Apply the proposed budgets

- At 5 Mbps, target 1080p UI animation at 24 presented frames per second or more during the animation workload.
- At 2 Mbps, target usable scrolling at 15 presented frames per second or more. Count actual changing frames, not duplicate decoded frames.
- At 1 Mbps, keep the text readable at native capture resolution and target input-to-visible-response p95 below 500 ms in the echo workload.
- At 0.5 Mbps, prefer current readable updates over smoothness. Show degradation explicitly and recover without stale input replay.
- In every profile, require no lost or duplicated key and button transitions during a completed session.
- During a ten-second network interruption, stop accepting control, release held remote input, and establish a fresh session after reconnection.
- Target remote-input cleanup within three seconds after a silent connection failure. Socket closure should trigger cleanup immediately.
- After the route returns, target a current usable desktop within five seconds. Include any required full refresh or keyframe in that interval.
- While the desktop is static, target less than 20 KB per second of app payload after the initial refresh. Keepalive traffic is part of this budget.
- Target host CPU use below one full core on average during the 1080p desktop workload with hardware encoding. Report fallback CPU use separately.
- Compare local browser responsiveness with streaming stopped and active. Investigate a p95 regression above 20 percent before accepting the design.
- Require bounded memory, image queues, and input queues throughout a 30-minute mixed workload. Declare the implementation's limits before running it.

These budgets do not guarantee every page fits a given bitrate. A failure must identify the affected workload and limiting resource. Prefer tuning the validated design before adding another transport.

## Check the real controls

| Scenario | Required observable result |
| --- | --- |
| Same desktop | A window moved on Linux changes position in Comet. A remote move changes the physical monitor. |
| Browser testing | Open an existing Linux app, reproduce a bug in the Linux browser, and inspect the same session locally. |
| Text entry | Enter punctuation, shifted keys, repeated keys, shortcuts, and non-ASCII text without unwanted substitutions. |
| Mac keyboard | Verify Command, Control, Option, and Super behavior. Document shortcuts Comet or macOS reserves and provide explicit remote shortcut controls where needed. |
| Pointer geometry | Hit small targets at corners and the center after scaling, fullscreen, and viewport resize. |
| Dragging | Press, drag outside the rendered desktop, and release without a stuck button. |
| Clipboard | Copy and paste bounded UTF-8 text in both directions through explicit actions. Verify denied permission, oversize text, and reconnect behavior. |
| Controller takeover | A second authenticated tab cannot mix input with the active controller. Takeover clears previous held input. |
| Stale display | A deliberately stalled changed image shows a stale state and prevents unsafe control. A healthy unchanged desktop stays usable. |
| Old video generation | Delay old media and acknowledgments across reconnect, lock, and geometry changes. Old content never re-enables control in the new generation. |
| Disconnect | Drop the connection while a modifier or button is held. Verify release on the Linux desktop. |
| Authentication | Rejected users, origins, sessions, and replayed capabilities cannot capture the desktop or inject input. |
| Existing service | The original port 443 root still reaches the service on local port 3000. |

## Validate remote unlock and lifecycle

1. Establish remote viewing while Linux is already unlocked.
2. Validate the lock-state query and measure transition observation latency. Query failure produces Unknown and disables clipboard.
3. Lock Linux through the normal user action. Verify the remote stream shows the lock screen rather than retaining desktop content.
4. Reconnect from Comet while Linux is locked.
5. Unlock through the standard lock-screen input path. Do not bypass the locker or change its authentication policy.
6. Verify that clipboard transfer is unavailable while locked and previous clipboard actions do not replay after unlocking. Include locking during an exchange.
7. Verify that logs and receipts contain no passwords, clipboard bodies, or raw keystrokes. Do not record the password-entry segment.
8. Exercise monitor power-off, a changed output mode, monitor removal, compositor restart, and suspend followed by local resume.
9. Confirm capture resources recover or the client reports a clear unavailable state. Do not claim access while the machine is suspended.

Run disruptive lifecycle scenarios serially in a dedicated test session. Do not let simultaneous agents control the shared live desktop. If locked-session capture fails, stop this milestone and revisit the supported mechanism. Do not disable the lock screen to pass it.

## Keep the evidence

Keep run manifests, counters, redacted logs, screenshots, and short demonstrations under a local receipts directory. Capture before and after each milestone. A test result includes its exact commit and environment.

Use the control-ui workflow for browser behavior and control-cli for host startup and shutdown. Use benchmark-checklist before reporting comparative performance. Mark results VERIFIED, NOT VERIFIED, or INCONCLUSIVE. A proposed test or a compiled program cannot be marked VERIFIED.

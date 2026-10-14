# Low-bandwidth development run

ScreenPier must keep the same Linux desktop usable when the route slows. The current fixed quality modes are the reference for comparison. Previous automatic bitrate selectors failed the 0.5 Mbps route-change test, so a new selector must first explain where old video waits.

## Evidence that sets the priority

An archived isolated cold-start Low data run at 0.5 Mbps, 200 ms added round-trip delay, and 2 percent random loss preserved exact native-pixel 14 px text OCR with no proxy queue drops. Its host readiness log nevertheless recorded 35 display-expiry falls and 13.8 seconds of unavailable control after first becoming ready during the roughly 49-second session; the longest completed gap was 1.6 seconds. The one-second browser samples showed only 3/18 Waiting observations during the constrained phase and 7/20 after restoration. Thus occasional samples and a readable screenshot are insufficient evidence of reliable input. These numbers describe one historical Linux Chromium fixture run, not the current candidate or Mac Comet.

The separate failed Smooth-to-Low run stalled for the full 15-second handover check and dropped 49 packets at the proxy. During its initial 5 Mbps Smooth phase, capture-to-encoder output was around 30–50 ms while acknowledged frames later became more than one second old. The first implementation unit orders lease retirement before the replacement offer. Candidate diagnostics now record the WebRTC send queue at regular samples and at readiness changes. Together with proxy and browser timing, those samples can locate where video waits; queue snapshots alone cannot distinguish network delay from browser playout delay. Neither correction has a measured performance gain yet.

## Acceptance for this run

The primary local test starts with 5 Mbps, changes to 0.5 Mbps with 200 ms added round-trip delay and 2 percent random packet loss, then restores 5 Mbps. The test uses the physical 1920 by 1080 Hyprland output through an isolated host and browser. A candidate passes only if each of five runs shows current video within five seconds of the route change, exact native-pixel reading of the 14 px fixture text, no lost or duplicate input transitions, and no unbounded queue growth. Count every readiness fall and its duration from host events, including falls between browser samples. The test records presented frames, readiness, packet drops, transport statistics, host stage timings, and the binary hash. Alternate baseline and candidate runs. Report medians and ranges, including failures.

The separate 1 Mbps input test uses 150 ms added round-trip delay and 1 percent loss. It requires exact literal key delivery before reporting input-to-visible latency. Synthetic shaping does not include Tailscale overhead or prove Mac Comet performance. The final acceptance path remains the actual Mac browser and tailnet route in [validation](validation.md).

## Work sequence

1. Repair the isolated test scripts after the ScreenPier rename. Preserve the private service and create fresh receipts for the current release binary.
2. Measure fixed Low data from startup and Smooth-to-Low switching under the same route. Separate encoder output delay, sender packet timing, proxy queue time, and browser presentation delay.
3. Compare at least two designs against the measured bottleneck. Reject a design that depends on increasing the stale-display input deadline to hide delayed video.
4. Implement the smallest candidate that improves the measured path. Keep each code change and its behavioral check separate.
5. Repeat the full tests, inspect the exact built artifact, and document accepted and rejected results. Leave the installed private service unchanged until a separate deployment decision.

The first two steps are diagnostic. They can find that no candidate passes this run. A failed or incomplete run stays marked **NOT VERIFIED** or **INCONCLUSIVE**, rather than becoming a performance claim.

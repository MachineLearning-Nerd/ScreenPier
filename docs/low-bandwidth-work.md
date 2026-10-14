# Low-bandwidth development run

ScreenPier must keep the same Linux desktop usable when the route slows. The current fixed quality modes are the safe starting point. Previous automatic bitrate selectors failed the 0.5 Mbps route-change test, so a new selector must first explain where old video waits.

## Acceptance for this run

The primary local test starts with 5 Mbps, changes to 0.5 Mbps with 200 ms added round-trip delay and 2 percent random packet loss, then restores 5 Mbps. The test uses the physical 1920 by 1080 Hyprland output through an isolated host and browser. A candidate passes only if each of five runs shows current video within five seconds of the route change, exact native-pixel reading of the 14 px fixture text, no lost or duplicate input transitions, and no unbounded queue growth. The test records presented frames, readiness, packet drops, transport statistics, host stage timings, and the binary hash. Alternate baseline and candidate runs. Report medians and ranges, including failures.

The separate 1 Mbps input test uses 150 ms added round-trip delay and 1 percent loss. It requires exact literal key delivery before reporting input-to-visible latency. Synthetic shaping does not include Tailscale overhead or prove Mac Comet performance. The final acceptance path remains the actual Mac browser and tailnet route in [validation](validation.md).

## Work sequence

1. Repair the isolated test scripts after the ScreenPier rename. Preserve the private service and create fresh receipts for the current release binary.
2. Measure fixed Low data from startup and Smooth-to-Low switching under the same route. Separate encoder output delay, sender packet timing, proxy queue time, and browser presentation delay.
3. Compare at least two designs against the measured bottleneck. Reject a design that depends on increasing the stale-display input deadline to hide delayed video.
4. Implement the smallest candidate that improves the measured path. Keep each code change and its behavioral check separate.
5. Repeat the full tests, inspect the exact built artifact, and document accepted and rejected results. Leave the installed private service unchanged until a separate deployment decision.

The first two steps are diagnostic. They can find that no candidate passes this run. A failed or incomplete run stays marked **NOT VERIFIED** or **INCONCLUSIVE**, rather than becoming a performance claim.

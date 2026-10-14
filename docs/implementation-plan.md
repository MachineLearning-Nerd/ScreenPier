# Prepare and build ScreenPier

Provide private access from Mac Comet to the existing Linux desktop. Start with desktop control and text clipboard, then verify remote unlock and operation under constrained bandwidth. Preserve the existing Tailscale service.

This plan stops before implementation until the user requests execution. The preparation artifacts describe the contract. Runtime experiments settle codec and transport choices.

## Read the preparation documents

- Read [requirements](requirements.md) for confirmed scope and proposed defaults.
- Read [technical evidence](evidence.md) for observed facts and source constraints.
- Read [architecture](architecture.md) for the design comparison and provisional base.
- Read [validation](validation.md) for test scenarios and proposed performance budgets.

Run the implementation as a bounded Feature workflow under poteto-mode, with figure-it-out for the experiment loop and local decision trail. Commit, PR creation, installation, service configuration, and external exposure require the user's requested execution scope. No automatic publishing or merging is part of this plan.

## Preparation worklist

- [x] Read the Principles section of the poteto-mode skill.
- [x] Phase A: Frame. Establish workflow, scope, and feasibility constraints.
- [x] Phase B: Design the workflow. Compare two structural candidates, cross-review them, and record the synthesis.
- [x] Phase C: Run the loop. Verify preparation artifacts only. Runtime prototypes are deferred until execution is requested.
- [x] Phase D: Keep the audit trail. Record decisions and evidence locally.
- [x] Phase E: Verify and hand back. Review the actual documents and identify remaining runtime gates.

[Preparation review](preparation-review.md) records the findings addressed, final document verdict, and runtime attention items.

## Throughput checkpoint

Product requirements and source research are complete. Capture, hardware encoding, Comet negotiation, and locked-session access block technical certainty. Independent design and document review can proceed now. The live desktop, GPU, and Tailscale service are shared state. Run disruptive experiments serially. The smallest safe implementation unit is a local capture check, followed by an independently verified input check.

## Start the execution phase

- [ ] Confirm the current workspace and preserve any work added since preparation.
- [ ] Recheck project instructions and current Hyprland, Tailscale, GPU, and browser versions.
- [ ] Establish a local repository and the desired delivery scope if the user requests it.
- [ ] Add Rust formatting, lint, build, and test commands appropriate to the first unit.
- [ ] Build a repeatable fixture and receipts directory before measuring performance.
- [ ] Keep native dependency choices explicit and use the current graphical session's environment.

The setup is complete when another engineer can repeat the local checks and identify the host session without altering desktop configuration.

## Prove capture and input locally

- [ ] Capture eDP-1 through a working Wayland path and verify colour, geometry, cursor, and freshness.
- [ ] Establish a simple SHM correctness baseline before GPU-import optimisation.
- [ ] Measure actual damage behavior and compare application change detection with codec prediction if needed.
- [ ] Create virtual input devices in the intended existing seat.
- [ ] Track remote held keys and buttons with one owner.
- [ ] Implement cleanup on normal stop, error, disconnect, and controller replacement.
- [ ] Reject input from expired sessions and stale geometry generations.
- [ ] Verify keyboard mapping and absolute pointer coordinates through a controlled local test page, with cleanup already in place.
- [ ] Check capture and ordinary virtual input through the normal lock screen in a dedicated local test session before investing in the media pipeline.

The milestone passes when capture and remote input demonstrably affect the same physical desktop, with reliable cleanup. Preserve screenshots and input-test receipts. Capture and input must pass separately before combining them. Local lock-screen feasibility must pass here. Locked reconnect from the actual Mac remains a later acceptance gate.

## Establish an owner only browser test endpoint

- [ ] Configure the owner identity locally before starting any endpoint.
- [ ] Implement loopback HTTP admission, expected host and origin checks, and short-lived session capabilities.
- [ ] Verify owner rejection and capability revocation in local tests before forwarding desktop content.
- [ ] Prepare a separate private HTTPS Serve recipe that preserves port 443's existing root.
- [ ] Review and apply that recipe only within the authorised execution scope.
- [ ] Authenticate the Comet client before enabling any captured desktop media.
- [ ] Bind the negotiated media connection to that authenticated session and restrict its listener to the tailnet where applicable.
- [ ] Keep remote input disabled until current-generation display progress is verified.

The milestone passes when only the configured owner can enter the desktop test session and the existing service still works. Every Mac transport comparison below uses this admission boundary. No anonymous desktop streaming prototype is required.

## Compare stream delivery on Comet

- [ ] Validate one video encoder and one fallback encoder without claiming that adapter enumeration proves GPU support.
- [ ] Validate a WebRTC media route through Tailscale and actual Comet negotiation.
- [ ] Use lossless image updates to compare text clarity and sparse-update traffic. Do not run a full motion comparison on the stop-and-wait sketch.
- [ ] If further image testing is justified, revise its bounded scheduling before comparing motion. Record the new dependency and acknowledgment rules.
- [ ] Verify Comet's RTP presentation metadata, host timestamp mapping, and rejection of old-generation video acknowledgments before admitting input.
- [ ] Compare both candidates using the same workloads and isolated profiles in validation.md.
- [ ] Record bitrate, text readability, changing frames presented, input-to-visible-response, CPU, queue depth, and failure counts.
- [ ] Select one production transport from the evidence. Keep the alternate as a test reference unless the measured failure requires a fallback.
- [ ] Record whether str0m or a GStreamer integration provides the validated video route, if video wins.

The milestone passes when the selected path reaches the actual Mac browser and the comparison has reproducible correctness and performance evidence. If neither passes the proposed budgets, report the limiting resource and revisit the design before building a second production path.

## Finish browser controls and session recovery

- [ ] Expand the already authenticated test client into the desktop interface.
- [ ] Recheck owner, origin, capability, media, and input admission after integration changes.
- [ ] Preserve port 443's root mapping to local port 3000.
- [ ] Implement one active remote controller and explicit takeover.
- [ ] Include pointer position with click events.
- [ ] Bound queues and pause input when the displayed state is stale or geometry mismatches.
- [ ] Provide clear connecting, active, degraded, unavailable, and reconnecting states.
- [ ] Validate fullscreen, resize, scrolling, dragging, and Mac keyboard behavior.

The milestone passes when Comet controls the live desktop and rejected connections cannot view or control it. The original Tailscale service must still work.

## Add text clipboard and remote unlock

- [ ] Add explicit copy and paste actions with bounded UTF-8 text.
- [ ] Handle denied browser permissions and reject oversized messages.
- [ ] Prevent diagnostic logs from collecting clipboard content or raw input.
- [ ] Connect while Linux is locked and unlock through the ordinary locker.
- [ ] Stop clipboard actions while locked and prevent replay after unlock.
- [ ] Recreate capture state after geometry or graphical-session changes.
- [ ] Test monitor power state, suspend followed by local resume, and graphical-session restart.
- [ ] Keep pre-login and remote power-on outside the supported behavior.

The milestone passes only with actual locked-session reconnect and unlock evidence. Failure is a blocker for the confirmed first-version requirement. Do not replace it with a claim based on advertised protocols.

## Verify normal use and failure recovery

- [ ] Complete every applicable scenario in validation.md on the exact release candidate.
- [ ] Test normal Linux browser work alongside streaming under each constrained profile.
- [ ] Verify the selected adaptation policy and bounded memory during a 30-minute mixed workload.
- [ ] Verify silent loss, explicit disconnect, reconnect, and held-input cleanup.
- [ ] Record the actual direct or relayed Tailscale route.
- [ ] Run independent read-only review of the code and receipts.
- [ ] Fix supported findings and rerun affected checks.
- [ ] Drive the host CLI and browser client through their real entry points.
- [ ] Provide a tested start and stop recipe, supported-version notes, and a local stop command.
- [ ] Add graphical-login startup only after manual start, stop, and recovery are reliable.

The release candidate passes when the user can operate Linux from Comet, test an app in the Linux browser, leave evidence locally, reconnect through lock, and meet the agreed budgets. Unit tests, live browser evidence, and performance evidence are all required.

## Separate implementation from preparation

No application code, runtime prototypes, network exposure, or desktop settings are part of the current preparation. The next user instruction can authorise local prototype execution, broader implementation, or revisions to these documents. Preparation itself does not authorise installation or service changes.

# TailWindowAccess preparation review

The final independent review passed the preparation documents. This verdict covers requirements, proposed architecture, test design, prerequisite ordering, and the local decision trail. It does not establish working remote desktop software.

## Design comparison

Independent scoring favored the WebRTC sketch at 7 of 10 over the image-update sketch at 5 of 10. These scores describe the written designs, not performance measurements.

The image sketch's one-outstanding-update policy has a round-trip-dependent cadence limit. The synthesis rejects it for the proposed motion budgets. A lossless image path remains useful for a narrow text-quality comparison.

The provisional WebRTC base inherits explicit display generations and recovery semantics. Its presentation contract uses sender RTP timestamps and browser frame metadata. Comet must validate that contract before remote input is admitted.

## Findings addressed

- The plan requires remote held-input cleanup before local input and lock experiments.
- The plan requires owner-only HTTPS admission and media binding before any Mac desktop-stream comparison.
- Local lock-screen feasibility precedes media-pipeline investment. Mac locked reconnect remains a later acceptance gate.
- Comet's frame-specific presentation acknowledgments must match the active lease, transport epoch, and display generation.
- Owner bootstrap is local. Restart, stop, takeover, and policy replacement revoke affected sessions.
- Hyprland's locked query is the selected state source. Unknown state disables clipboard. Polling latency and transition races remain tests.

## Document checks

Local checks passed for Markdown links, balanced code fences, prohibited punctuation, prerequisite ordering, and audit evidence paths. The final read-only reviewer found no material preparation defect. The reviewer used the current chat's model, as required by pstack-harness. This was independent same-model review.

The existing Tailscale Serve root still points to 127.0.0.1:3000 on port 443. Preparation created documents and a local audit trail. It did not install software, configure services, capture the desktop, inject input, or run a streaming benchmark.

## Attention

Actual capture, input, GPU encoding, Comet interoperability, presentation correlation, private media connectivity, clipboard behavior, and remote unlock remain runtime gates. Performance budgets are proposed targets. The user must request the execution phase before prototypes or application implementation start.

The plan follows Experience First by prioritising actual desktop use, Model the Domain by assigning controller and held-input ownership, Exhaust the Design Space by comparing two structural sketches, Sequence Work into Verifiable Units by ordering prerequisites before dependent experiments, and Prove It Works by requiring live and measured receipts before runtime claims.

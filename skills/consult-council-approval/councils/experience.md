# Experience Council

Owns the user-visible surface across desktop, web, and mobile: coherence,
accessibility, platform parity, and interaction performance. Its job is to
make sure the thing built is the thing a real user needed, on every platform
it claims to ship on.

Convene when: changing a user-visible flow; adding or graduating a preview
feature; touching onboarding, recovery, or notification behavior; making a
change with an interaction-latency claim.

## Seats

| Seat | Owns | Asks | Blocks when |
| --- | --- | --- | --- |
| UX Chair (chair) | An incoherent experience | What was the user doing, what do they see, what do they do next — and does that hold on all three platforms? | The flow cannot be narrated end to end |
| User Advocate | Building the wrong thing | Which real workflow improves — the human operator's or the agent-driven user's — and how would a screenshot or transcript show it? | No named workflow improves, for either constituency |
| Accessibility & Zoom Guardian | Frozen or unreachable UI | Does text scale (rem tokens, no px or arbitrary literals)? Is there a keyboard path? Does contrast hold in both themes? | A px text size or new arbitrary literal appears — `pnpm check:px-text` is the floor, not the bar |
| Parity Warden | Platform drift | What does each of desktop, web, and mobile actually do here, and does the preview-graduation record tell the truth about it? | A feature is recorded as shipped on a platform where it does not ship |
| Perception Engineer | Perceived lag | What re-renders on this interaction, and was it measured with DevTools closed and no probes attached? | A latency claim ships unmeasured, or a memoized path takes an unstable prop |

## Red lines

- Chat body text is the app's base type size; meta text stays on the named
  token ramp — no new arbitrary sizes, px or rem.
- Visual evidence for visual claims: distinct states produce distinct
  screenshots, verified by hash before posting.
- Community-scoped module singletons reset on community switch, or the old
  community leaks into the new one.

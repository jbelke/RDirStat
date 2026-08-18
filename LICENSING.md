# Licensing

RDirStat is dual-licensed. Pick the track that matches what you are doing.

| | Open source track | Commercial track |
| --- | --- | --- |
| License | **AGPL-3.0-only** (see [LICENSE](LICENSE)) | Commercial license from the copyright holder |
| Price | Free | Negotiated |
| Your source code | Must be published under AGPL-3.0 | Stays yours and closed |
| Network/SaaS use | Must offer full source to your users (AGPL §13) | No disclosure obligation |
| Attribution | Required, and cannot be removed | Required, terms negotiated |

Copyright (C) 2026 Joshua Belke — <https://github.com/jbelke>.

## The open source track (AGPL-3.0-only)

Use it, read it, modify it, fork it, and ship it, for free and forever, as long
as you play by copyleft:

- **Keep the credit.** The copyright notice, [LICENSE](LICENSE), and [NOTICE](NOTICE)
  travel with every copy and every derivative.
- **Publish your changes.** Anything you distribute that is built on this code is
  itself AGPL-3.0-only, in full — not just the files you touched.
- **The network counts as distribution.** If you let anyone interact with a
  modified version over a network, AGPL §13 obliges you to offer them the
  complete corresponding source. This is the clause plain GPL lacks, and it is
  the reason this project uses AGPL: a hosted "disk insights" service built on
  this code is a derivative work, not a loophole.

If that suits you, you never have to talk to anyone. Just build.

## The commercial track

Buy a license if you want to do any of the following:

- Ship this code, or a derivative, inside a **closed-source or proprietary**
  product.
- Run a **hosted or SaaS** offering built on it without publishing your source.
- Redistribute it under terms **other than** AGPL-3.0-only (including bundling it
  into a product whose license is incompatible with copyleft).
- Have the **attribution requirements relaxed** or renegotiated.
- Get a **warranty, indemnity, or support commitment** — the AGPL track is
  explicitly "as is," with no warranty (LICENSE §§15–16).

Commercial licenses are granted by the copyright holder only.

**Contact:** Joshua Belke — joshbelke@gmail.com — <https://github.com/jbelke>

Include what you are building, whether it is distributed or hosted, and rough
scale. Terms are negotiated per deal; there is no click-through.

## Contributions

To keep the commercial track possible, the copyright holder must be able to
license the whole work. Two rules follow, and they are not negotiable:

1. **Contributors sign a CLA.** By opening a pull request you agree to license
   your contribution under AGPL-3.0-only **and** to grant Joshua Belke a
   perpetual, irrevocable right to relicense it, including under commercial
   terms. Sign off your commits (`git commit -s`) to state this on the record.
2. **Never copy third-party copyleft code into this repository.** This matters
   more here than in an ordinary AGPL project: pasted AGPL or GPL code belongs
   to its author, so it cannot be included in a commercial license — a single
   copied function would poison the commercial track for the whole file, and
   possibly the binary. Re-implement behaviour from documentation and tests;
   never lift source text.

## SPDX

```
SPDX-License-Identifier: AGPL-3.0-only
```

Use `AGPL-3.0-only`, not `AGPL-3.0-or-later`: the "or later" form lets a future
FSF license govern this code, which would hand away control over the terms the
commercial track is carved out of.

---

This document explains the licensing intent in plain language. Where it and
[LICENSE](LICENSE) disagree about the open source track, LICENSE governs. It is
not legal advice — have a lawyer review any commercial agreement before signing.

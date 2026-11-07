# Modern Python bytecode: evidence and bounded behavior contracts

Static inspection on 2026-11-07 revisited the exact AceStream 3.2.11 package
used by earlier research. No original module was imported or executed. The
contracts below describe the inspected code objects; runtime selection between
Python and native implementations remains unverified.

## Reproducible package identity

Official package:
[`acestream_3.2.11_ubuntu_22.04_x86_64_py3.10.tar.gz`](https://download.acestream.media/linux/acestream_3.2.11_ubuntu_22.04_x86_64_py3.10.tar.gz).

- Archive SHA-256:
  `9b6bbd76a55e5a434641afae3b9cf8e6154ce1cf392152ec3aed5ac265432b2e`.
- Nested `lib/modules.zip` SHA-256:
  `35bd257f8570a66594ffe49a9a7e0c4254278c8c933e968531458a5ab0470ae6`.
- The ZIP contains 1,584 ordinary Python 3.10 `.pyc` files: 260 in `Core`,
  1,138 in `PluginsContainer`, 63 in `third_party` and 123 elsewhere under
  `ACEStream`. Counts do not measure runtime importance or source ownership.
- The outer package also contains seven core `.so` files, six with Cython
  0.29.22 markers. A Python bytecode decompiler does not recover their native
  implementations.
- Both package metadata and parsed `version.pyc` identify version 3.2.11.

Across 54 nested ZIP/egg/wheel containers, no entries had ZIP encryption flags;
all 1,849 `.pyc` headers used the Python 3.10 magic. Eleven selected engine
modules parsed with xdis 6.1.8 into 608 code objects, and four additional package
modules also parsed. This proves ordinary static access to those samples,
not absence of every other protection or importer in the product.

## Decompiler acceptance

`uncompyle6` 3.9.3 rejected Python 3.10 input despite returning status zero.
`pycdas` from pycdc revision `b428976` disassembled all 15 samples, but none of
the ten substantive sampled modules was accepted as equivalent decompiled
source. Nine had errors, incomplete output, invalid syntax or a crash.

The remaining `timeshift.pyc` produced syntax-valid output with status zero,
but independent xdis inspection showed wrong operands and call targets. At
offsets 58/60/62 in `_find_piece_by_timestamp`, the actual comparison loads
`first` and `last`; the decompiled expression instead used `None` and `last`.
At offset 90, the original call target is `equal`, not `None`. Empty output
from the crashed HLS decompilation also passed a bare syntax parser.

Process status and syntax checks are therefore insufficient. Check the relevant
instruction operands, branch destinations and state changes before deriving a
behavioral contract. Do not copy generated source into Outpace.

## Current live selection

Module: `ACEStream/Core/Video/PiecePickerStreaming.pyc`, SHA-256
`e6343ff6bf5c1aca47ec7ce608382068e2ce693ddb3d943d313fdc71f97b3c52`.

The `next_new` code object starts at source line 571. Byte offsets below are
within that code object, not source lines:

- Offsets 98–128: live mode without a selected live start position returns no
  candidate.
- Offsets 130–222: live mode with a connection invokes `rarest_first` on that
  connection's `interesting` candidates, the availability counts and `wantfunc`,
  passing **`pick_first=True`**, then returns its result.
- In the `rarest_first` helper, offsets 84–96 return the first filtered iterator
  candidate when that flag is true. This branch does **not** compare availability
  counts to find the rarest candidate. Numeric piece order additionally depends
  on how the candidate collection was populated and is not established here.
- The priority-range logic later in `next_new` is not reached by that live-with-
  connection return. Do not describe every path as a three-zone rarity policy.

The same module's `check_outstanding_requests` exposes a rate-aware cancellation
policy: a minimum request age is checked before urgent/prebuffer eligibility;
eligible zero-rate requests are cancelled; other eligible requests compare
cumulative outstanding bytes divided by short-term rate with a remaining
budget. Playing and buffering modes use different delay tuples. The playing
branch also consults `piece_due`; its time domain must be established at the
producer before using that expression as a Rust deadline specification.

Useful synthetic cases are: no live start position; empty eligible candidates;
first eligible candidate differing from the rarest; minimum-age boundary;
zero rate; and two queued requests whose cumulative ETA crosses the budget.
These describe isolated policy cases, not verified active-engine behavior or
a reason to replace Outpace's bounded sequential scheduler wholesale.

## HLS manifest and segment updates

Module: `ACEStream/Core/DASH/HLS.pyc`, SHA-256
`bf83bc02b6957684746850cea6cb97da05f38202d992fbe9b96d1809ec2d8397`.

`_rewrite_urls` iterates master playlist variants and replaces each variant URI
through `_rewrite_url(stream_id)`. It is not a general key/map/segment URI
rewriter. Relative-reference normalization may occur elsewhere and was not
established by this helper.

The `_update_segments` code object starts at source line 810:

- Offsets 168–212 reset the stream if the incoming last sequence is below the
  previously recorded last sequence. The incoming last sequence is calculated
  from `media_sequence + segment_count - 1`; an empty segment list returns early.
- Offsets 484–518 skip an already indexed piece before inspecting its URI.
  Offsets 520–572 also skip a sequence at or below the current last sequence
  when past pieces are disallowed. These are sequence-based identity decisions,
  not proof of changed-URI detection.
- Offsets 574–588 explicitly reject a nonempty segment byte-range field.
- Offsets 736–758 retain the segment URI and duration in maps indexed by piece
  sequence. Subsequent code clears the URI on the retained segment object.

Synthetic cases worth retaining in Outpace include sequence rollback, the same
URI at a new sequence, a changed URI at an old sequence, an empty update and
explicit/implicit byte ranges. Match each desired contract to current Rust
tests. Outpace already handles range predecessor identity and source epochs;
reproducing the observed Python rejection or stale-URI behavior would regress
those capabilities. The original implementation is a comparison source, not
an unconditional correctness specification.

## Archive and implementation boundary

The inspected `timeshift.pyc` accepts supplied timestamps and piece bounds and
searches for a matching piece. It does not establish remote retention, archive
acquisition, timestamp units or UTC programme catch-up. Outpace's retained HLS
replay and bounded peer-history probing have their own limits described in
[timeshift documentation](../../timeshift.md).

Derive small behavioral specifications and synthetic inputs from confirmed
control flow, implement them independently, and preserve ownership, memory,
request and time bounds. Do not weaken current functionality merely to mimic
an observed historical limitation. Consult native call paths separately before
claiming that a Python policy governs the original running engine.

# Native live timing, underrun and request reconciliation

Static inspection on 2026-11-07 followed three selected Cython paths in the
official AceStream 3.2.11 Linux x86-64 Python 3.10 package. This complements
[the Python bytecode contracts](73-modern-python-bytecode.md). It does not
establish runtime selection between duplicate Python and native paths.

## Evidence boundary

The [official archive](https://download.acestream.media/linux/acestream_3.2.11_ubuntu_22.04_x86_64_py3.10.tar.gz)
has SHA-256
`9b6bbd76a55e5a434641afae3b9cf8e6154ce1cf392152ec3aed5ac265432b2e`.
The selected `live.so` has SHA-256
`79c5429bb9180c8c599cc3456681c81e907b9423b30b3c984371453bdcff574a`.

The library was read as data, never imported or executed. Named method
wrappers, constructor assignments, vtable initialization, selected interned
identifiers and native instructions corroborate the interpretations below.
Addresses use Ghidra image base `0x100000`; subtract it for ELF virtual
addresses. Decompiled source and vendor binaries are not part of Outpace.

## Queue-aware service estimates

`SingleDownload.get_active_requests` (`0x20cd70`) returns the keys of its active
request dictionary. `get_avg_chunk_download_time` (`0x299ab0`) returns the
average at object offset `0xf8`. In the eligible path,
`get_piece_download_time` (`0x6673c0`) estimates the time as:

```text
(active requests + wanted chunks) * average chunk service interval
```

Availability and range gates can instead return an unavailable sentinel.
Those sentinel numbers are not meaningful durations to transplant into Rust.

`got_piece_chunk` (`0x56b3f0`) looks up the corresponding active request and
forms a sample using `monotime() - max(request_timestamp, previous_chunk_arrival)`.
The same body stores its entry clock snapshot into the second boundary at offset
`0x88` after processing the chunk, identifying its producer rather than inferring
the meaning from an offset. A separate lower-bound branch exists; its constant
is not transplanted into Outpace.
The callback at SingleDownload vtable offset `0x28` resolves to `0x2b8ef0`:
it maintains a sum, count and rolling history of **100 samples**, then divides
the sum by the count. This is an arithmetic rolling mean, not an exponential
moving average. The helper also records the last sample at offset `0xb0`, which
the estimator checks before using measured timing. The `monotime` import is
from `ACEStream.Core.Utilities.utilities`. Static parsing of that exact
package's `utilities.pyc` establishes an unscaled call to `time.monotonic()`:
the samples are fractional seconds, with an undefined clock origin and only
differences meaningful. This does not establish runtime selection of native
versus Python implementations.

Outpace schedules whole pieces. Its independent adaptation measures accepted
whole-piece service intervals with monotonic `Instant` and `Duration`, retaining
at most 100 samples per active peer. An interval starts at the later of request
admission and the preceding measured completion, excluding idle time. Only a
complete accepted piece attributable to its still-outstanding sole owner can
teach the estimator. Signature validation remains descriptor-dependent;
unsigned streams are not described as cryptographically authenticated.

Unrequested or duplicate data, mixed contributors, ambiguous retries and
partial pieces retained across reconnects do not teach a service interval.
Choke and expired measurements return the estimator to unknown. Freshness uses
the existing request timeout, without creating a second timeout policy.

The estimated finish cost is `(queued pieces + 1) * mean interval`, using
checked arithmetic. Existing spare-capacity and bounded-window priorities stay
primary: timing only resolves ties between otherwise equal peers, and only
when **all** those peers have fresh estimates. Equal capacity means the queue
factor is equal within this tie group. This deliberately does not replace the
capacity-first scheduler with a global minimum-ETA picker. Unknown or overflowed
estimates preserve the exact previous deterministic selection.

## Underrun and prebuffering

`LiveClientTransporter.available` (`0x215840`) returns its buffered byte count.
`pop` (`0x3dbf70`) removes a queued payload, subtracts its length and updates a
rate measurement. This independently identifies bytes rather than guessing
from a field name.

`update_prebuffering` (`0x3fa600`) has two successful paths:

- The current piece exists and `download_rate_func() >= bitrate * 1.3`.
- Every piece in the requested prebuffer range exists.

Otherwise it records missing pieces, applies their configured priority and
reports `(required pieces - missing pieces) / required pieces`. On success it
sets progress to 1.0, clears the prebuffer-active state, calls `notify_playable`
and wakes waiting readers while holding its condition lock. Constructor
assignments confirm `bitrate`, `piecelen` and `download_rate_func`; the inspected
constructor alone does not prove bits/s versus bytes/s at every caller.

The nested underrun predicate (`0x22e470`) returns false while buffered bytes
remain. When empty, absent retained metadata gives true; otherwise it compares
the refill clock minus metadata `local_ts` with **1.0**, using strict
greater-than. Refill converts `time.time()` to an unsigned integer, while the
metadata writer (`0x44bda0`) stores a floating-point `time.time()` value and
initializes this retained metadata only when it is absent. Its complete reset
lifecycle has not been traced. This is not a proven "one second since the last
packet" rule, nor the monotonic clock used for service timing.

With underrun handling enabled, the enclosing refill path requires both an
underrun and a false `sustainable(False)` result before resetting progress to
zero and requesting pause with autoresume enabled. With playback and pausing
enabled, entering pause (`0x381300`) sets the paused and autoresume flags,
increments a pause sequence, and emits a player callback with `"pause"` and
the `autoresume` and `seq` fields. An already-paused call updates the autoresume
flag without entering pause again. Recovery therefore coordinates with player
state; it is distinct from initial prebuffer readiness.

The scheduled callback (`0x1d38a0`) calls `autoresume(sustainable)` after a
**0.3** delay. `autoresume` (`0x2b0850`) stops unless playback, paused and
autoresume flags remain set. It invokes the supplied test function: false
schedules another check after 0.3; true calls the resume body (`0x4e25c0`),
whose inspected instructions clear paused and autoresume flags under the
playback/paused/pausing-enabled guards. Additional resume side effects have
not been reconstructed.

`sustainable` (`0x4d4660`) normally counts contiguous available pieces `C`
starting at the playback head, stopping at the first missing piece or the
configured window length `N`. It succeeds when `C >= N`, or when `C >= M`
and `download_rate_func() >= bitrate * 1.3`. The constructor derives `N` and
`M` with `time_to_pieces(high_range_time)` and
`time_to_pieces(high_range_time_min)`, respectively; these are separate from
the initial `max_prebuf_packets` target.

There is also a full-window maintenance path. Every call increments a counter;
when `update_progress` is true and the counter exceeds 10, it resets the
counter, scans all `N` pieces, reprioritizes missing pieces and updates progress
from the number available. This branch succeeds only when the whole window
is available, without the fast-rate alternative. The ordinary path can
reprioritize its first hole when the relevant option is enabled. The public
wrapper defaults `update_progress` to true, while the pre-pause check passes
false. Refill sets the counter to 10 when requesting pause, so the first
scheduled sustainability check enters the full-window path. These constants
and callbacks describe the inspected native path; they are not new Outpace
configuration defaults.

Outpace's source pump is shared among viewers. Restarting global startup
buffering on every runtime gap would pause all of them. Its existing
consumer-specific keyframe gates and HLS generation/discontinuity recovery
remain separate. The defect fixed here is narrower: an upstream that repeatedly
returns immediately-ready empty chunks could monopolize the executor. Both
startup collection and released pass-through now yield on empty input after
preserving stats and pending discontinuities. Nonempty media gains no extra
yield, buffer or delay; the first-media deadline and one-way release remain.
An expired deadline now wins over an immediately-ready source read, so retained
media drains before a newly ready source gap is consumed. The next payload
still carries that gap. Previously the reservoir already bounded empty reads
at expiry; this additional change makes deadline arbitration deterministic.

## Seek and stale requests

`on_live_seek` (`0x4c1c80`) enumerates active request triples, cancels those below
the seek target or above a bound returned by its picker, then requests more.
The upper-bound helper (`0x783b00`) has additional availability and mode gates;
its result must not be equated with a peer's advertised maximum without tracing
those gates.

`cancel_request` (`0x416250`) sends a wire cancel, removes the active triple,
removes its chunk index from the grouped request collection and deletes an
empty group. `check_outstanding_pieces` (`0x57c800`) compares request ages with
a threshold, with a separate configured threshold before timing samples exist.
It skips choked alternatives. Retaining the same candidate has a different
bookkeeping path; choosing an alternative cancels expired requests and invokes
`send_priority_request`. An argument of 5 to the candidate provider is visible,
but its policy meaning is not established by that literal.

Outpace does not introduce an unverified wire-cancel implementation. It fixes a
confirmed stale-work gap: after draining contiguous completed pieces, the
shared monotonic request floor is published immediately, before downstream
output can block. A retry already queued on another peer then stops requesting
obsolete chunks even without a new live-head announcement. A network write
already in progress may finish; later stale requests are suppressed. Viewer
seek does not move the shared upstream producer.

## Qualification and remaining limits

Synthetic regression coverage includes cold/stale timing, rolling-history
bounds, idle exclusion, overflow, admission rejection, mixed ownership,
retries, retained partial data, cancellation, media deadlines and actual duplex
request backpressure. The empty-input and contiguous-drain regressions fail on
the preceding implementation.

These are bounded independently implemented corrections. They do not recover
the entire original engine, establish provider archive acquisition or UTC
catch-up, or prove faster startup on real channels. Full native player feedback
and global ETA scheduling need separate end-to-end qualification before policy
changes. See [timeshift limits](../../timeshift.md) for existing archive behavior.

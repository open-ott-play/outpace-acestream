# Selected upstream compatibility and responsiveness changes

This downstream adaptation follows the November 2026 review of
[jamezrin/outpace](https://github.com/jamezrin/outpace). It preserves our managed
origin HLS, retained replay, source-budget ownership and native recovery work.
The upstream and downstream release numbers are independent.

## Identity and protection

[Upstream PR 290](https://github.com/jamezrin/outpace/pull/290) includes nine
optional live descriptor identity fields in their protocol order, preserving
present zero and empty values. The prior downstream formula included only
`provider_key` and `provider_sid`. Live identity and legacy wrapped-VOD identity
must remain distinct; metadata unrelated to identity must not change the hash.

[Upstream PR 314](https://github.com/jamezrin/outpace/pull/314) classifies the
recognized descriptor's authoritative protection field before playback. This
is adapted across our additional origin-HLS routing, VOD and cache paths.
Public, protected and malformed metadata remain distinguishable. Inspection
must not establish catalog authority or grant playback permission.

## Bounded data movement

[PR 315](https://github.com/jamezrin/outpace/pull/315) replaces peer receive-prefix
moves and temporary read copies with bounded `BytesMut` reads. Framing limits,
partial-read cancellation and per-I/O deadlines remain unchanged.
[PR 318](https://github.com/jamezrin/outpace/pull/318) lends decoded live payloads
to reassembly while retaining the independent owned API.
[PR 319](https://github.com/jamezrin/outpace/pull/319) contributes one-buffer wire
encoding; this downstream already used lazy unknown-message construction.
Our startup packet scanner already borrowed complete TS packets, so PR 316
requires no duplicate implementation.

## Admission and responsive peer control

[PR 294](https://github.com/jamezrin/outpace/pull/294) provides optional shared
provider admission. Our adaptation counts pending acquisition and source/content
owners, retaining VOD ownership through range readers. Zero remains unlimited.
Its count is not a claim that every asynchronous socket has finished closing.
Managed origin HLS keeps its independent relay cap; source admission does not
replace the existing byte budgets. See [configuration](../../configuration.md).

[PR 298](https://github.com/jamezrin/outpace/pull/298) and
[PR 301](https://github.com/jamezrin/outpace/pull/301) inform bounded reciprocal
read and output-capacity operations. The adaptation retains accepted media
across pool replacement, preserves order/discontinuities and grants productive
credit only after output capacity is obtained. Existing accepted-piece timing,
stale-request cancellation and authentication policy remain applicable. Buffer
statistics are scanned with bounded per-turn work; native `livepos.buffer_complete`
distinguishes an unfinished measurement from a known empty buffer. Compatibility
keys remain unchanged.

## Scope and qualification

This is selective adaptation, not an upstream tree replacement. Persistent
headless support sources, private-peer mode, a replacement DHT library and the
new audio-normalization foundation are outside this change. No production
network setting, source selection, buffer target or deployed image is changed.

Synthetic regression tests and compilation establish their tested contracts,
not faster real-channel startup, universal playback correctness or original
engine parity. Actual channel performance requires a controlled same-source
comparison with the deployed player and network path.

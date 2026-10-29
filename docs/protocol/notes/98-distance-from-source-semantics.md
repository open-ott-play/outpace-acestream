# Reconciliation: `distance_from_source`

**Status:** evidence reconciliation; no wire-code behavior changed.

The repository contains two historical descriptions of a source peer's
`mi.distance_from_source`: note 11 labels `0` as the broadcaster, and note 26
records an earlier Outpace-origin profile using `0`. A later official-engine local-source
capture in [note 32](32-official-consumer-requests-but-rejects-pieces.md)
records the source profile as `-1`. These are historical observations from
different experiments; they do not establish one universal protocol rule.

The current implementation follows the later observed source-profile value:

- `crates/ace-swarm/src/seed.rs` advertises `distance_from_source: -1` from its
  outbound source handshake.
- `crates/ace-swarm/src/live_session/peer.rs` compares this value only after
  `max_piece` and `position`; at that tie-break, lower signed values rank first.
- The `ace-wire` tests exercise integer encoding and decoding. They do not prove
  that every external Acestream implementation assigns identical semantics to
  `0` and `-1`.

Accordingly, the implementation currently emits `-1`, matching the later
recorded source-profile observation; this does not by itself prove successful
interoperability. The meaning of the field across all peers remains open.
Preserve the older notes as experiment history. Do not alter the producer or
scheduler from an LLM-generated interpretation; change this contract only with
a retained raw handshake and a repeatable official-consumer interoperability
result.

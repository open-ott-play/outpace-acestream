# BitTorrent lineage and the mixed Python/native engine

This historical research note was corrected on 2026-11-07. Its earlier claim
that the Linux engine contains no pure `.pyc` was false: it did not account for
the nested `lib/modules.zip`. The earlier inference of a direct BitTornado fork
from class names was also stronger than the evidence supports.

## What the evidence establishes

- The inspected AceStream 3.2.11 Linux/Python 3.10 package contains **1,584 `.pyc`
  files** in `lib/modules.zip`, including 260 under `ACEStream/Core` and 43 under
  `Core/BitTornado`. Seven core native libraries coexist with this bytecode; six
  carry Cython markers. See [the modern bytecode inspection](73-modern-python-bytecode.md)
  for exact provenance, bounded function contracts and decompiler limitations.
- Native strings in `live.so` include `Connecter`, `ConnecterConnection`,
  `Encrypter`, `Downloader`, `DownloaderTransporter`, `DownloaderFeedback`,
  `Rerequester`, `handshake` and `peer_id`. `CoreApp.so` also contains handshake
  and `Encrypter` names. These locate relevant native components; they do not
  establish exclusive ownership of the protocol or runtime dispatch between
  native and Python implementations.
- An [official TorrentStream 1.0.2 traceback](https://forum.acestream.media/t/oshbika/2076)
  contains `Tribler/.../*.pyc` paths. [Tribler's history](https://tribler.org/TriblerGettingStarted/)
  documents its ancestry through ABC, BitTornado and the original BitTorrent
  implementation. Together these establish historical lineage, without proving
  the direct fork point or the proportion of modern AceStream derived from it.
- `Encrypter` is a historical module name, not evidence of an MSE/AES design.
  The inspected historical implementation handles connection framing and the
  handshake. Native names such as `m2_AES_encrypt` or `import_crypto.pyx` need
  separate call-path evidence before assigning them a protocol or importer role.

## What remains useful

The public BitTorrent/BitTornado sources explain the base handshake, bitfield,
have, request, piece, cancel and tracker structures. They provide a reference
for independent implementation and comparison of Ace-specific behavior. Check
the license and provenance of each upstream component before copying source;
the application wrapper's license does not automatically cover the engine.

The Python modules in the inspected current package are now a more direct
reference for selected policies than an older upstream release. They can be
statically parsed without running the engine. A function's name is insufficient:
the current live caller passes `pick_first=True` to a helper named
`rarest_first`, which returns the first eligible iterator candidate in that
mode. It does not minimize rarity on that branch.

## Historical questions versus current implementation

The original note treated identity, transport hashing and live download as
unresolved initial tasks. Those statements are historical, not the current
Outpace backlog. Later work records [identity acceptance](18-identity-gate-passed.md),
[live download](19-live-download-working.md),
[transport hash derivation](29-infohash-formula-cracked.md) and
[source header timestamps](33-live-piece-header-timestamp-and-official-consumer.md).
The current `ace-wire`, `ace-peer` and `ace-swarm` implementations already contain
the corresponding codecs and live-session machinery.

Further work should compare individual observed policies with the current
Rust implementation and synthetic boundary cases. Standard bytecode availability
does not prove which path executes in a running engine, complete API parity,
or a provider archive. Native Cython code still requires separate analysis.

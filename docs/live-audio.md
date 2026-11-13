# Live audio compatibility

ATSC AC3 (PMT type 0x81) is recognized alongside DVB AC3 descriptors. ATSC E-AC3 (0x87) is recognized but remains unsupported on its own; a supported alternate audio track may be selected. Conflicting codec signals are rejected.

Normalization classification scans at most 16 MiB of evidence without retaining discarded pre-opening packets, under the existing startup deadline. Stable generations lasting at least ten seconds do not consume the rapid-recovery allowance. Three rapid recoveries per rolling minute remain the limit.

The 1 MiB input budget bounds detached copies, replay and stdin queue allocations. Views up to 512 KiB are detached so small views do not pin large source owners. Larger source chunks are consumed in place and retain their source owner until consumed or cancelled. This is not a total source-memory or RSS bound: source backing allocation accounting remains the producer's responsibility.

Adapted from Outpace upstream commits 31c6b34 and 774140b, preserving the original project authorship and AGPL notices.

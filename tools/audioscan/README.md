# Audio-language inventory

This optional Python 3.9+ command-line tool identifies streams whose original
MPEG-TS program map advertises audio in more than one language. It retains the
useful inventory, snapshot, comparison and history features of the experimental
scanner with bounded requests and safe existing-daemon behavior.

It inspects metadata. A match does **not** prove successful decoding, independent
viewer audio switching, or that the stream remains available later.

## Input and a first scan

Provide explicit values on standard input as `cid<number>=<content-id>` lines.
Bare identifiers receive numeric `#` labels. The tool never opens the repository's
private identifier registry. Do not script reads of that registry. Only numeric
labels are accepted; channel names and arbitrary labels cannot enter snapshots.

The following identifier is synthetic and will not resolve to a real stream:

```sh
printf '%s\n' 'cid1=0123456789abcdef0123456789abcdef01234567' \
  | python3 tools/audioscan/scan.py --base-url http://127.0.0.1:8621 --details
```

Use operator-supplied real values only in private runtime input. Never commit
identifiers, names, private reports, logs or captures. Standard output contains
matching labels; diagnostics contain labels and fixed status names.

## Existing and isolated engines

`--base-url` accepts an HTTP origin on a loopback IP or `localhost`. Remote hosts,
credentials, paths and queries are rejected. Probes use the original raw route
`/streams/ace/cid:<id>.ts`; no HLS playlist or audio selection is requested.

**The scanner never sends DELETE requests.** Closing the GET response releases
only its own viewer. An existing engine's cache/reaper decides when upstream work
can stop. Concurrent probes open separate streams, so use smaller batches and
modest concurrency when sharing an engine with active viewers.
`--no-stop` is retained as a compatibility no-op.

`--start PATH_TO_OUTPACE` launches a private engine instead of using `--base-url`.
It uses fresh loopback ports, its own temporary data directory, no inbound serving
or port mapping, and zero startup prebuffer. It clears inherited `OUTPACE_*`
settings before applying those explicit options. Peer fan-out retains the engine
default; scanner concurrency does not increase per-stream peer limits. Successful runs and failed
startup both terminate and reap the child. A child that ignores termination is
killed and reaped. `--keep-workdir` retains the private data and log after a
successful startup; failed startup removes it and never prints raw engine logs.

## Limits and interpretation

- `--workers N`: 1–32 concurrent probes, default 5. At most 10,000 input entries.
- `--deadline SECONDS`: finite value greater than zero and at most 120, default 20.
  The wall-clock deadline includes connection, headers and body; a socket watchdog
  interrupts slow header/body trickles. Each probe also stops after 8 MiB.
- Ctrl-C cancels queued probes and waits for currently active, deadline-bounded
  probes before cleaning up a private child.
- `--aac-only`: require two **ADTS AAC** tracks with different declared languages.
  LATM and private AAC are inventory codecs, not candidates for native ADTS
  selection. This flag still does not promise decoding or viewer switching.
- `--details`: display the observed codec/language list for every input label.

The parser checks PSI lengths, CRC, current/next state, descriptors, program identity
and continuity. Packet loss, transport-error and scrambling indicators cannot
complete a partial section. Duplicate packets are ignored; pointer-field tails
can complete the previous section. The lowest nonzero program number in a complete
single-section PAT is selected, regardless of order. Multi-section PATs are
reported as `unsupported-pat-sections`; multi-section PMTs are not accepted. The
first valid selected PMT ends the probe. This is a deliberately bounded snapshot,
not a monitor of every program or later language changes. ISO 639 descriptors
use their first three-letter language entry; `und` does not count as a language.

A `no-pmt` result can mean a timeout, inaccessible stream, unsupported/malformed
PSI, or delayed prebuffer. It is not proof that a channel is dead.

## Private snapshots and comparisons

```sh
# During a scan with operator-supplied stdin:
python3 tools/audioscan/scan.py --summary /private/path/latest.json \
  --history /private/path/history

# Read-only operations; no engine or stdin is used:
python3 tools/audioscan/scan.py --diff /private/path/before.json /private/path/after.json
python3 tools/audioscan/scan.py --history /private/path/history --trend cid1
python3 tools/audioscan/scan.py --history /private/path/history --changes --since 7d
```

Snapshots contain numeric labels, status, codec, language and PID; never supplied
content identifiers. Files are written atomically with private permissions.
History files use UTC timestamps and collision suffixes. Invalid or oversized
snapshots are ignored. Comparisons distinguish missing coverage from changed
probe status and warn when the selection criterion changes.

`--watch --history DIR` performs **one scan**, stores its snapshot, and reports
changes in the last 24 hours (override with `--since` / `--until`). It does not run
as a background scheduler. Exit status 3 means this snapshot changed a label's
matching state; 0 means normal completion, 2 means invalid input/data, 130 means
interrupted. A failed probe is recorded as an observation, not a command failure.

## Offline tests

```sh
python3 -m unittest discover -s tools/audioscan -v
```

Tests include synthetic PSI, snapshot/history behavior, child cleanup mocks and
loopback HTTP servers for normal responses and slow header/body deadlines. They
never query real channels or open the private registry.

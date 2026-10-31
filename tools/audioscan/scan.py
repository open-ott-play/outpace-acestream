#!/usr/bin/env python3
"""Find live streams whose audio tracks span more than one language.

The scanner reads ``label=content-id`` lines (or bare 40-hex ids) on stdin, probes each
stream's raw MPEG-TS through the engine's native route, parses the PAT/PMT the engine
forwards there and prints **only the registry labels** that carry audio in two or more
languages. Diagnostics go to stderr; no content id, infohash or stream name is printed.

The tool never opens the gitignored registry: the operator supplies identifiers on
stdin. See README.md for usage, method, limitations and identifier hygiene.
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import http.client
import ipaddress
import json
import math
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import threading
import urllib.parse
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone

TS_PACKET = 188
TS_SYNC = 0x47
PAT_PID = 0x0000
MAX_SECTION = 1024

# Elementary stream types that carry audio, and the codec name reported for each.
AUDIO_STREAM_TYPES = {
    0x03: "mpeg1",
    0x04: "mpeg2",
    0x0F: "aac",
    0x11: "aac-latm",
    0x81: "ac3",  # ATSC AC-3
    0x85: "dts",  # Blu-ray DTS-HD / DTS core
    0x87: "eac3",  # ATSC E-AC-3
}

# A private-data (stream_type 0x06) stream is only audio when one of these says so.
PRIVATE_AUDIO_DESCRIPTORS = {
    0x6A: "ac3",
    0x7A: "eac3",
    0x7B: "dts",
    0x7C: "aac-private",
}

LANGUAGE_DESCRIPTOR = 0x0A
AAC_CODECS = frozenset({"aac"})
LABEL_PATTERN = re.compile(r"(?:cid[0-9]{1,8}|#[0-9]{1,8})\Z")
MAX_PROBE_BYTES = 8 * 1024 * 1024
ID_PATTERN = re.compile(r"[0-9a-fA-F]{40}")
SUMMARY_SCHEMA = 1
HISTORY_NAME = re.compile(r"^(\d{8}T\d{6}Z)(?:-(\d+))?\.json$")
RELATIVE_BOUND = re.compile(r"^(\d+)([hd])$")


@dataclass(frozen=True)
class Track:
    codec: str
    language: str | None
    pid: int


@dataclass
class Result:
    label: str
    tracks: list[Track] | None
    note: str

    @property
    def hit(self) -> bool:
        return bool(self.tracks) and is_hit(self.tracks)


@dataclass(frozen=True)
class Transition:
    """One changed field of one key between two stored snapshots."""

    when: str
    key: str
    field: str
    before: str
    after: str


def mpeg_crc32(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte << 24
        for _ in range(8):
            crc = ((crc << 1) ^ (0x04C11DB7 if crc & 0x80000000 else 0)) & 0xFFFFFFFF
    return crc


def valid_section(section: bytes, table_id: int, minimum: int) -> bool:
    return (
        len(section) >= minimum
        and section[0] == table_id
        and section[1] & 0xF0 == 0xB0
        and 3 + (((section[1] & 15) << 8) | section[2]) == len(section)
        and section[5] & 0xC1 == 0xC1
        and mpeg_crc32(section) == 0
    )


class Sections:
    """Bounded PSI assembly, preserving the previous section's pointer-field tail."""

    def __init__(self) -> None:
        self.buffer = bytearray()

    def _append(self, data: bytes) -> list[bytes]:
        self.buffer.extend(data)
        result = []
        while len(self.buffer) >= 3:
            if self.buffer[0] == 0xFF:
                self.buffer.clear()
                break
            total = 3 + (((self.buffer[1] & 15) << 8) | self.buffer[2])
            if total < 7 or total > MAX_SECTION:
                self.buffer.clear()
                break
            if len(self.buffer) < total:
                break
            result.append(bytes(self.buffer[:total]))
            del self.buffer[:total]
        return result

    def feed(self, payload: bytes, pusi: bool) -> list[bytes]:
        if not pusi:
            return self._append(payload) if self.buffer else []
        if not payload or payload[0] + 1 > len(payload):
            self.buffer.clear()
            return []
        boundary = 1 + payload[0]
        result = self._append(payload[1:boundary]) if self.buffer else []
        self.buffer.clear()
        return result + self._append(payload[boundary:])


class Probe:
    """Validated single-program PSI inventory. Select the lowest PAT program number.

    A multi-section PAT is deliberately unsupported: selecting from only its first
    received fragment would make program choice depend on arrival order.
    """

    def __init__(self) -> None:
        self.buffer = bytearray()
        self.sections: dict[int, Sections] = {}
        self.continuity: dict[int, tuple[int, bytes]] = {}
        self.program: tuple[int, int] | None = None
        self.tracks: list[Track] | None = None
        self.note = "no-pmt"

    @property
    def done(self) -> bool:
        return self.tracks is not None

    def push(self, chunk: bytes) -> bool:
        self.buffer.extend(chunk)
        while len(self.buffer) >= TS_PACKET:
            self._align()
            if len(self.buffer) < TS_PACKET:
                break
            packet = bytes(self.buffer[:TS_PACKET])
            del self.buffer[:TS_PACKET]
            self._packet(packet)
            if self.done:
                return True
        return self.done

    def _align(self) -> None:
        offset = 0
        while len(self.buffer) - offset >= TS_PACKET:
            offset = self.buffer.find(TS_SYNC, offset)
            if offset < 0:
                self.buffer.clear()
                return
            if len(self.buffer) - offset < 2 * TS_PACKET or self.buffer[offset + TS_PACKET] == TS_SYNC:
                break
            offset += 1
        if offset:
            del self.buffer[:offset]

    def _packet(self, packet: bytes) -> None:
        pid = ((packet[1] & 31) << 8) | packet[2]
        if pid != PAT_PID and (self.program is None or pid != self.program[1]):
            return
        assembler = self.sections.setdefault(pid, Sections())
        if packet[1] & 0x80 or packet[3] & 0xC0:
            assembler.buffer.clear()
            self.continuity.pop(pid, None)
            return
        control = (packet[3] >> 4) & 3
        if control == 0:
            assembler.buffer.clear()
            return
        start = 4
        if control & 2:
            size = packet[4]
            if size > (182 if control & 1 else 183):
                assembler.buffer.clear()
                self.continuity.pop(pid, None)
                return
            start = 5 + size
            if size and packet[5] & 0x80:
                assembler.buffer.clear()
                self.continuity.pop(pid, None)
        if not control & 1:
            return
        cc = packet[3] & 15
        previous = self.continuity.get(pid)
        self.continuity[pid] = (cc, packet)
        if previous:
            if previous == (cc, packet):
                return
            if cc != (previous[0] + 1) % 16:
                assembler.buffer.clear()
        for section in assembler.feed(packet[start:], bool(packet[1] & 0x40)):
            if pid == PAT_PID:
                self._on_pat(section)
            else:
                self._on_pmt(section)

    def _on_pat(self, section: bytes) -> None:
        if not valid_section(section, 0, 12):
            return
        if section[6:8] != b"\x00\x00":
            self.note = "unsupported-pat-sections"
            self.program = None
            self.sections.clear()
            self.continuity.clear()
            return
        if (len(section) - 12) % 4:
            return
        programs = {}
        for offset in range(8, len(section) - 4, 4):
            program = int.from_bytes(section[offset:offset + 2], "big")
            pid = ((section[offset + 2] & 31) << 8) | section[offset + 3]
            if program:
                if pid < 0x20 or pid == 0x1FFF or program in programs:
                    return
                programs[program] = pid
        selected = min(programs.items()) if programs else None
        if selected != self.program:
            self.program = selected
            self.sections = {0: self.sections.get(0, Sections())}
            self.continuity = {k: v for k, v in self.continuity.items() if k == 0}
        self.note = "no-pmt"

    def _on_pmt(self, section: bytes) -> None:
        if not valid_section(section, 2, 16) or self.program is None:
            return
        if int.from_bytes(section[3:5], "big") != self.program[0] or section[6:8] != b"\x00\x00":
            return
        end = len(section) - 4
        offset = 12 + (((section[10] & 15) << 8) | section[11])
        if offset > end:
            return
        tracks = []
        seen = set()
        try:
            parse_descriptors(section[12:offset])
            while offset < end:
                if offset + 5 > end:
                    return
                stream_type = section[offset]
                pid = ((section[offset + 1] & 31) << 8) | section[offset + 2]
                length = ((section[offset + 3] & 15) << 8) | section[offset + 4]
                next_offset = offset + 5 + length
                if next_offset > end or pid in seen or pid < 0x20 or pid == 0x1FFF:
                    return
                seen.add(pid)
                track = classify(stream_type, parse_descriptors(section[offset + 5:next_offset]), pid)
                if track is not None:
                    tracks.append(track)
                offset = next_offset
        except ValueError:
            return
        self.tracks = tracks
        self.note = "ok"


def parse_descriptors(data: bytes) -> list[tuple[int, bytes]]:
    out = []
    offset = 0
    while offset < len(data):
        if offset + 2 > len(data):
            raise ValueError("incomplete descriptor")
        tag, length = data[offset:offset + 2]
        end = offset + 2 + length
        if end > len(data):
            raise ValueError("incomplete descriptor body")
        out.append((tag, data[offset + 2:end]))
        offset = end
    return out


def classify(stream_type: int, descriptors: list[tuple[int, bytes]], pid: int) -> Track | None:
    codec = AUDIO_STREAM_TYPES.get(stream_type)
    private = {PRIVATE_AUDIO_DESCRIPTORS[tag] for tag, _ in descriptors if tag in PRIVATE_AUDIO_DESCRIPTORS}
    if len(private) > 1:
        raise ValueError("conflicting codec descriptors")
    if stream_type == 6 and private:
        codec = next(iter(private))
    if codec is None:
        return None
    if stream_type != 6 and private and codec not in private:
        if private != {"aac-private"} or codec not in {"aac", "aac-latm"}:
            raise ValueError("contradictory codec descriptors")
    language = None
    for tag, body in descriptors:
        if tag == LANGUAGE_DESCRIPTOR:
            if not body or len(body) % 4:
                raise ValueError("invalid language descriptor")
            value = body[:3]
            if value.isalpha() and value.isascii():
                language = value.decode("ascii").lower()
                if language == "und":
                    language = None
            break
    return Track(codec=codec, language=language, pid=pid)


def declared_languages(tracks: list[Track]) -> set[str]:
    return {track.language for track in tracks if track.language}


def is_hit(tracks: list[Track]) -> bool:
    """Two or more audio tracks that declare at least two different languages."""
    return len(tracks) >= 2 and len(declared_languages(tracks)) >= 2


def is_aac_hit(tracks: list[Track]) -> bool:
    """Two or more ADTS AAC tracks; an inventory, not a decoding/switching proof."""
    aac = [track for track in tracks if track.codec in AAC_CODECS]
    return len(aac) >= 2 and len(declared_languages(aac)) >= 2


def read_entries(stream) -> list[tuple[str, str]]:
    entries: list[tuple[str, str]] = []
    seen_labels = set()
    for number, raw in enumerate(stream, start=1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        label = None
        value = line
        if "=" in line:
            label, value = (part.strip() for part in line.split("=", 1))
        elif len(line.split(None, 1)) == 2:
            label, value = (part.strip() for part in line.split(None, 1))
        value = value.split()[0] if value.split() else ""  # drop a trailing note
        if not ID_PATTERN.fullmatch(value):
            print(
                f"scan: line {number} skipped: expected a 40-hex content id",
                file=sys.stderr,
            )
            continue
        label = label or f"#{len(entries) + 1}"
        if not LABEL_PATTERN.fullmatch(label) or label in seen_labels:
            print(f"scan: line {number} skipped: invalid or duplicate label/identifier", file=sys.stderr)
            continue
        if len(entries) >= 10000:
            raise SystemExit("scan: at most 10000 entries per run")
        seen_labels.add(label)
        entries.append((label, value.lower()))
    return entries


def require_loopback(host: str) -> str:
    if host in ("localhost", ""):
        return "127.0.0.1"
    try:
        address = ipaddress.ip_address(host)
    except ValueError as error:
        raise SystemExit("scan: --base-url host must be a loopback IP literal") from error
    if not address.is_loopback:
        raise SystemExit("scan: refusing to probe a non-loopback daemon")
    return str(address)


def split_base(base: str) -> tuple[str, int]:
    try:
        parsed = urllib.parse.urlsplit(base if "://" in base else f"http://{base}")
        if parsed.scheme != "http" or parsed.username or parsed.password or parsed.path not in ("", "/") or parsed.query or parsed.fragment:
            raise ValueError()
        host = require_loopback(parsed.hostname or "")
        port = parsed.port or 8621
    except ValueError:
        raise SystemExit("scan: --base-url must be a loopback HTTP origin") from None
    return host, port


@contextmanager
def deadline_connection(host: str, port: int, deadline: float):
    """Close a specific connected socket at a total deadline, including header trickles."""
    stop_at = time.monotonic() + deadline
    connection = http.client.HTTPConnection(host, port, timeout=deadline)
    watchdog = None
    try:
        connection.connect()
        sock = connection.sock
        def expire():
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        remaining = stop_at - time.monotonic()
        if remaining <= 0:
            raise TimeoutError()
        watchdog = threading.Timer(remaining, expire)
        watchdog.daemon = True
        watchdog.start()
        yield connection, stop_at
    finally:
        if watchdog is not None:
            watchdog.cancel()
            watchdog.join()
        connection.close()


def probe_stream(host: str, port: int, hexid: str, deadline: float) -> tuple[list[Track] | None, str]:
    """GET only; closing the response releases only this viewer's subscription."""
    probe = Probe()
    try:
        with deadline_connection(host, port, deadline) as (connection, stop_at):
            connection.request("GET", f"/streams/ace/cid:{hexid}.ts")
            with connection.getresponse() as response:
                if response.status != 200:
                    return None, f"http-{response.status}"
                received = 0
                while time.monotonic() < stop_at and received < MAX_PROBE_BYTES:
                    chunk = response.read1(min(65536, MAX_PROBE_BYTES - received))
                    received += len(chunk)
                    if not chunk or probe.push(chunk):
                        break
    except (OSError, http.client.HTTPException):
        pass
    return probe.tracks, probe.note


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Daemon:
    """An isolated ``outpace serve`` owned by this run, on a private port and data dir."""

    def __init__(self, binary: str, workers: int) -> None:
        self.workdir = tempfile.mkdtemp(prefix="outpace-audioscan-")
        self.log_path = os.path.join(self.workdir, "daemon.log")
        self.proc = None
        try:
            self.port = free_port()
            env = {key: value for key, value in os.environ.items() if not key.startswith("OUTPACE_")}
            env.update(
                {
                    "OUTPACE_BIND": f"127.0.0.1:{self.port}",
                    "OUTPACE_RTMP_BIND": f"127.0.0.1:{free_port()}",
                    "OUTPACE_PEER_LISTEN": f"127.0.0.1:{free_port()}",
                    "OUTPACE_DATA_DIR": self.workdir,
                    "OUTPACE_ENABLE_INBOUND": "0",
                    "OUTPACE_ENABLE_PORT_MAPPING": "0",
                    # Without this every cold stream waits for its 30 s start buffer before the
                    # first byte, which makes a short probe look like an unreachable stream.
                    "OUTPACE_PREBUFFER_MS": "0",
                }
            )
            self._start(binary, env)
            self._await_ready()
        except BaseException:
            self.close(False)
            raise

    def _start(self, binary: str, env: dict) -> None:
        with open(self.log_path, "wb") as log:
            self.proc = subprocess.Popen(
                [binary, "serve"],
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
            )

    def _await_ready(self, timeout: float = 30.0) -> None:
        stop_at = time.monotonic() + timeout
        while time.monotonic() < stop_at:
            if self.proc.poll() is not None:
                raise SystemExit("scan: isolated daemon exited before readiness")
            try:
                with deadline_connection("127.0.0.1", self.port, min(2, max(0.01, stop_at - time.monotonic()))) as (connection, _):
                    connection.request("GET", "/healthz")
                    with connection.getresponse() as response:
                        if response.status == 200 and self.proc.poll() is None:
                            return
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(0.2)
        raise SystemExit("scan: isolated daemon did not become ready")

    def close(self, keep_workdir: bool) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        elif self.proc is not None:
            self.proc.wait()
        if keep_workdir:
            print(f"scan: kept run directory {self.workdir}", file=sys.stderr)
        else:
            shutil.rmtree(self.workdir, ignore_errors=True)


def key_order(label: str) -> tuple:
    suffix = re.search(r"(\d+)$", label)
    return (int(suffix.group(1)) if suffix else float("inf"), label)


def describe(tracks: list[Track]) -> str:
    return ", ".join(f"{track.codec}/{track.language or 'und'}" for track in tracks)


def summary_payload(results: list[Result], hits: list[str], mode: str) -> dict:
    """Snapshot of one run. Labels only: no content id, infohash or stream name."""
    return {
        "schema_version": SUMMARY_SCHEMA,
        "mode": mode,
        "hits": hits,
        "streams": {
            result.label: {
                "note": result.note,
                "tracks": None
                if result.tracks is None
                else [
                    {"codec": track.codec, "language": track.language, "pid": track.pid}
                    for track in result.tracks
                ],
            }
            for result in results
        },
    }


def load_summary(path: str) -> dict | None:
    try:
        with open(path, "r", encoding="utf-8") as handle:
            text = handle.read(10 * 1024 * 1024 + 1)
        if len(text) > 10 * 1024 * 1024:
            raise ValueError()
        document = json.loads(text)
    except FileNotFoundError:
        return None
    except (OSError, ValueError, RecursionError):
        print("scan: ignoring unreadable or oversized summary", file=sys.stderr)
        return None
    if not isinstance(document, dict) or document.get("schema_version") != SUMMARY_SCHEMA:
        print("scan: ignoring summary: unknown schema version", file=sys.stderr)
        return None
    streams = document.get("streams")
    hits = document.get("hits")
    codecs = set(AUDIO_STREAM_TYPES.values()) | set(PRIVATE_AUDIO_DESCRIPTORS.values())
    valid = document.get("mode") in ("any", "aac") and isinstance(streams, dict) and isinstance(hits, list)
    if valid:
        valid = all(isinstance(key, str) and LABEL_PATTERN.fullmatch(key) for key in streams)
        valid = valid and all(isinstance(key, str) and key in streams for key in hits)
    if valid:
        for entry in streams.values():
            if not isinstance(entry, dict) or not re.fullmatch(r"ok|no-pmt|unsupported-pat-sections|http-[1-5][0-9]{2}", str(entry.get("note", ""))):
                valid = False
                break
            tracks = entry.get("tracks")
            if tracks is None:
                continue
            if not isinstance(tracks, list):
                valid = False
                break
            for track in tracks:
                if not isinstance(track, dict) or not isinstance(track.get("codec"), str) or track.get("codec") not in codecs or type(track.get("pid")) is not int or not 0 <= track["pid"] < 8192:
                    valid = False
                    break
                language = track.get("language")
                if language is not None and (not isinstance(language, str) or not re.fullmatch(r"[a-z]{3}", language)):
                    valid = False
                    break
    if not valid:
        print("scan: ignoring invalid summary fields", file=sys.stderr)
        return None
    return document


def write_summary(path: str, payload: dict, *, replace: bool = True) -> None:
    directory = os.path.dirname(os.path.abspath(path))
    handle, temporary = tempfile.mkstemp(prefix=".audioscan-", dir=directory)
    try:
        with os.fdopen(handle, "w", encoding="utf-8") as out:
            json.dump(payload, out, indent=2, sort_keys=True)
            out.write("\n")
        # Labels are free of identifiers, but a run snapshot stays private evidence.
        os.chmod(temporary, 0o600)
        if replace:
            os.replace(temporary, path)
        else:
            os.link(temporary, path)
            os.unlink(temporary)
    except BaseException:
        try:
            os.unlink(temporary)
        except OSError:
            pass
        raise


def snapshot_notes(document: dict) -> dict[str, str]:
    streams = document.get("streams")
    notes = {}
    if isinstance(streams, dict):
        for label, entry in streams.items():
            if isinstance(entry, dict):
                notes[label] = str(entry.get("note", ""))
    return notes


def snapshot_labels(document: dict) -> set[str]:
    streams = document.get("streams")
    return set(streams) if isinstance(streams, dict) else set()


def report_delta(previous: dict, hits: list[str], notes: dict[str, str]) -> None:
    """Compare this run with a stored snapshot: which keys started or stopped matching."""
    before = {label for label in previous.get("hits", []) if isinstance(label, str)}
    now = set(hits)
    appeared = sorted(now - before, key=key_order)
    disappeared = sorted(before - now, key=key_order)
    kept = len(now & before)

    previous_notes = snapshot_notes(previous)
    changed = sorted(
        (label for label, note in notes.items() if label in previous_notes and previous_notes[label] != note),
        key=key_order,
    )

    if not appeared and not disappeared and not changed:
        print("scan: delta: no change in hits or probe status", file=sys.stderr)
        return
    if appeared or disappeared:
        print(
            f"scan: delta: appeared={' '.join(appeared) or '-'} "
            f"disappeared={' '.join(disappeared) or '-'} kept={kept}",
            file=sys.stderr,
        )
    if changed:
        flips = " ".join(f"{label}:{previous_notes[label]}->{notes[label]}" for label in changed)
        print(f"scan: delta: status changed: {flips}", file=sys.stderr)


def diff_snapshots(before_path: str, after_path: str) -> int:
    """Compare two stored snapshots offline: no probing, no daemon, no stdin."""
    for path in (before_path, after_path):
        if not os.path.exists(path):
            print(f"scan: no snapshot at {path}", file=sys.stderr)
            return 2
    before = load_summary(before_path)
    after = load_summary(after_path)
    if before is None or after is None:
        return 2

    print(f"scan: diff: {before_path} -> {after_path}", file=sys.stderr)
    if before.get("mode") != after.get("mode"):
        print(
            f"scan: delta: mode changed {before.get('mode')}->{after.get('mode')}; "
            "hit changes may reflect the criterion",
            file=sys.stderr,
        )
    # Batches are not always the same set of keys; say so rather than reporting the
    # difference as if both snapshots had scanned the same entries.
    only_before = sorted(snapshot_labels(before) - snapshot_labels(after), key=key_order)
    only_after = sorted(snapshot_labels(after) - snapshot_labels(before), key=key_order)
    if only_before:
        print("scan: diff: only in BEFORE: " + " ".join(only_before), file=sys.stderr)
    if only_after:
        print("scan: diff: only in AFTER: " + " ".join(only_after), file=sys.stderr)

    after_hits = [label for label in after.get("hits", []) if isinstance(label, str)]
    report_delta(before, after_hits, snapshot_notes(after))
    return 0


def write_history(directory: str, payload: dict, when: datetime | None = None) -> str:
    """Store a timestamped snapshot so successive runs can be read back as a series."""
    os.makedirs(directory, mode=0o700, exist_ok=True)
    base = (when or datetime.now(timezone.utc)).strftime("%Y%m%dT%H%M%SZ")
    for attempt in range(1, 100):
        name = f"{base}.json" if attempt == 1 else f"{base}-{attempt}.json"
        path = os.path.join(directory, name)
        try:
            write_summary(path, payload, replace=False)
            return path
        except FileExistsError:
            continue
    raise SystemExit(f"scan: too many snapshots for {base} in {directory}")


def snapshot_base(path: str) -> str | None:
    """The history stamp a stored snapshot path belongs to, or None if it is not one."""
    match = HISTORY_NAME.match(os.path.basename(path))
    return (match.group(1) + ("-" + match.group(2) if match.group(2) else "")) if match is not None else None


def read_history(directory: str) -> list[tuple[str, dict]]:
    """Load every stored snapshot, oldest first. Files that are not snapshots are skipped."""
    entries = []
    for name in os.listdir(directory):
        match = HISTORY_NAME.match(name)
        if match is None:
            continue
        document = load_summary(os.path.join(directory, name))
        if document is None:
            continue
        entries.append((match.group(1), int(match.group(2) or 1), document))
    entries.sort(key=lambda entry: (entry[0], entry[1]))
    return [(base if number == 1 else f"{base}-{number}", document) for base, number, document in entries]


def counted(count: int, noun: str) -> str:
    return f"{count} {noun}" if count == 1 else f"{count} {noun}s"


def format_stamp(base: str) -> str:
    return (
        f"{base[0:4]}-{base[4:6]}-{base[6:8]}"
        f"T{base[9:11]}:{base[11:13]}:{base[13:15]}Z"
    )


def parse_bound(
    raw: str, end_of_day: bool = False, now: datetime | None = None
) -> str | None:
    """Parse a date, timestamp or relative span into the history's stamp format."""
    text = raw.strip()
    relative = RELATIVE_BOUND.match(text)
    if relative:
        hours = int(relative.group(1)) * (24 if relative.group(2) == "d" else 1)
        moment = (now or datetime.now(timezone.utc)) - timedelta(hours=hours)
        return moment.strftime("%Y%m%dT%H%M%SZ")
    if text.endswith("Z"):
        text = text[:-1]
    for pattern in ("%Y-%m-%d", "%Y-%m-%dT%H:%M:%S", "%Y%m%dT%H%M%S"):
        try:
            moment = datetime.strptime(text, pattern)
        except ValueError:
            continue
        if pattern == "%Y-%m-%d" and end_of_day:
            moment = moment.replace(hour=23, minute=59, second=59)
        return moment.strftime("%Y%m%dT%H%M%SZ")
    print(
        "scan: cannot read date; use YYYY-MM-DD, an ISO timestamp, or a span "
        "like 24h or 7d",
        file=sys.stderr,
    )
    return None


def history_window(
    since_raw: str | None, until_raw: str | None
) -> tuple[str | None, str | None] | None:
    since = None
    until = None
    if since_raw:
        since = parse_bound(since_raw)
        if since is None:
            return None
    if until_raw:
        until = parse_bound(until_raw, end_of_day=True)
        if until is None:
            return None
    if since is not None and until is not None and since > until:
        print("scan: --since is after --until", file=sys.stderr)
        return None
    return since, until


def in_window(stamp: str, since: str | None, until: str | None) -> bool:
    moment = stamp[:16]
    return (since is None or moment >= since) and (until is None or moment <= until)


def warn_mixed_modes(snapshots: list[tuple[str, dict]], subject: str) -> None:
    modes = sorted({str(document.get("mode")) for _, document in snapshots})
    if len(modes) > 1:
        print(
            f"scan: {subject}: snapshots span modes {','.join(modes)}; matches mix criteria",
            file=sys.stderr,
        )


def key_state(document: dict, key: str) -> tuple[str, str] | None:
    """``(match, probe)`` for one key, or None when that run did not scan it."""
    streams = document.get("streams")
    entry = streams.get(key) if isinstance(streams, dict) else None
    if not isinstance(entry, dict):
        return None
    hits = document.get("hits")
    match = "yes" if isinstance(hits, list) and key in hits else "no"
    return match, str(entry.get("note", "-"))


def collect_transitions(
    snapshots: list[tuple[str, dict]],
) -> tuple[list["Transition"], set[str]]:
    """Every match/probe change between two observations of the same key."""
    previous: dict[str, tuple[str, str]] = {}
    changes: list[Transition] = []
    gaps: set[str] = set()
    for base, document in snapshots:
        present = snapshot_labels(document)
        # A key missing from a run breaks the comparison chain: its next appearance
        # starts a fresh baseline instead of being diffed against stale state.
        for key in [key for key in previous if key not in present]:
            gaps.add(key)
            del previous[key]
        for key in sorted(present, key=key_order):
            state = key_state(document, key)
            if state is None:
                continue
            before = previous.get(key)
            previous[key] = state
            if before is None:
                continue
            when = format_stamp(base)
            if before[0] != state[0]:
                changes.append(Transition(when, key, "match", before[0], state[0]))
            if before[1] != state[1]:
                changes.append(Transition(when, key, "probe", before[1], state[1]))
    return changes, gaps


def print_changes(
    directory: str,
    since_raw: str | None,
    until_raw: str | None,
    fresh: str | None = None,
) -> int:
    """Every key that changed its match or probe outcome inside a time window.

    ``fresh`` names the snapshot this run just wrote. A match transition recorded there
    means the pairing appeared or cleared since the previous run, so the run exits 3 and
    a scheduler can alert on it without parsing the table.
    """
    if not os.path.isdir(directory):
        print(f"scan: no history directory at {directory}", file=sys.stderr)
        return 2
    window = history_window(since_raw, until_raw)
    if window is None:
        return 2
    snapshots = [entry for entry in read_history(directory) if in_window(entry[0], *window)]
    if not snapshots:
        print(f"scan: no snapshots in {directory} for that period", file=sys.stderr)
        return 2
    warn_mixed_modes(snapshots, "changes")

    changes, gaps = collect_transitions(snapshots)
    for change in changes:
        print(
            f"{change.when:<21} {change.key:<9} {change.field:<6} "
            f"{change.before:<9} {change.after}"
        )

    span = (
        f"{counted(len(snapshots), 'snapshot')} "
        f"({format_stamp(snapshots[0][0])} .. {format_stamp(snapshots[-1][0])})"
    )
    if changes:
        keys = len({change.key for change in changes})
        print(
            f"changes: {counted(len(changes), 'transition')} across {counted(keys, 'key')} "
            f"in {span}",
            file=sys.stderr,
        )
    else:
        print(f"changes: no transitions in {span}", file=sys.stderr)
    if gaps:
        print(
            f"changes: {counted(len(gaps), 'key')} missing from some snapshots; gaps are "
            "coverage, not stream changes: " + " ".join(sorted(gaps, key=key_order)),
            file=sys.stderr,
        )
    # Preserve the filename suffix: two snapshots in one second are distinct runs.
    fresh_changes = []
    if fresh:
        index = next((i for i, (base, _) in enumerate(snapshots) if base == fresh), None)
        if index is not None and index > 0:
            fresh_changes, _ = collect_transitions(snapshots[index - 1:index + 1])
    fresh_matches = [change for change in fresh_changes if change.field == "match"]
    if fresh_matches:
        print(
            "changes: new match transition in this snapshot ("
            + ", ".join(f"{change.key} {change.before}->{change.after}" for change in fresh_matches)
            + "); exiting 3",
            file=sys.stderr,
        )
        return 3
    return 0


def print_trend(
    directory: str,
    key: str,
    since_raw: str | None = None,
    until_raw: str | None = None,
) -> int:
    """One key's match/probe history across a stored snapshot series."""
    if not os.path.isdir(directory):
        print(f"scan: no history directory at {directory}", file=sys.stderr)
        return 2
    window = history_window(since_raw, until_raw)
    if window is None:
        return 2
    snapshots = [entry for entry in read_history(directory) if in_window(entry[0], *window)]
    if not snapshots:
        print(f"scan: no snapshots in {directory} for that period", file=sys.stderr)
        return 2
    warn_mixed_modes(snapshots, "trend")

    print(f"{'when':<21} {'match':<6} {'probe':<9} tracks")
    matched = 0
    absent = 0
    last_change = None
    previous_state = None
    for base, document in snapshots:
        streams = document.get("streams")
        entry = streams.get(key) if isinstance(streams, dict) else None
        if not isinstance(entry, dict):
            match, note, tracks = "absent", "-", "-"
            absent += 1
        else:
            hits = document.get("hits")
            match = "yes" if isinstance(hits, list) and key in hits else "no"
            if match == "yes":
                matched += 1
            note = str(entry.get("note", "-"))
            observed = entry.get("tracks")
            tracks = "-"
            if isinstance(observed, list):
                described = ", ".join(
                    f"{item.get('codec')}/{item.get('language') or 'und'}"
                    for item in observed
                    if isinstance(item, dict)
                )
                tracks = described or "-"
        when = format_stamp(base)
        print(f"{when:<21} {match:<6} {note:<9} {tracks}")
        # Any change counts: a match flip and a probe that stopped resolving mean
        # different things, and both are what makes a series worth reading.
        state = (match, note)
        if previous_state is not None and state != previous_state:
            last_change = when
        previous_state = state

    summary = (
        f"trend: {key}: {counted(len(snapshots), 'snapshot')}, matched in {matched}, "
        f"absent from {absent}"
    )
    if last_change:
        summary += f", last change {last_change}"
    print(summary, file=sys.stderr)
    if absent == len(snapshots):
        print(f"scan: {key} never appears in this history; check the label", file=sys.stderr)
    return 0


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Report streams with audio tracks in several languages.",
        epilog=(
            "Identifiers arrive on stdin as 'label=content-id' (or bare content ids). "
            "stdout carries only the labels of matching streams."
        ),
    )
    parser.add_argument(
        "--base-url",
        default="http://127.0.0.1:8621",
        help="running daemon to probe (loopback only; default: %(default)s)",
    )
    parser.add_argument(
        "--start",
        metavar="OUTPACE",
        help="start an isolated daemon from this binary instead of using --base-url",
    )
    parser.add_argument(
        "--aac-only",
        action="store_true",
        help="only match two or more ADTS AAC tracks in different languages (inventory only)",
    )
    parser.add_argument(
        "--workers", type=int, default=5, help="streams probed at once (default: %(default)s)"
    )
    parser.add_argument(
        "--deadline",
        type=float,
        default=20.0,
        metavar="SECONDS",
        help="per-stream probe limit; the probe ends as soon as a PMT is parsed "
        "(default: %(default)s)",
    )
    parser.add_argument(
        "--no-stop",
        action="store_true",
        help="compatibility no-op: probes never delete sessions",
    )
    parser.add_argument(
        "--details",
        action="store_true",
        help="print the observed codec/language tracks per hit on stderr",
    )
    parser.add_argument(
        "--keep-workdir",
        action="store_true",
        help="keep a started daemon's data directory and log for diagnosis",
    )
    parser.add_argument(
        "--summary",
        metavar="PATH",
        help="write a labels-only run snapshot, and report hit/status changes against the "
        "snapshot already at PATH",
    )
    parser.add_argument(
        "--diff",
        nargs=2,
        metavar=("BEFORE", "AFTER"),
        help="compare two stored snapshots and print the drift; probes nothing and reads "
        "no stdin",
    )
    parser.add_argument(
        "--history",
        metavar="DIR",
        help="also store a dated snapshot in DIR after a scan; with --trend, the series to "
        "read",
    )
    parser.add_argument(
        "--trend",
        metavar="KEY",
        help="print one key's history from the --history DIR series; probes nothing and "
        "reads no stdin",
    )
    parser.add_argument(
        "--changes",
        action="store_true",
        help="print every key that changed its match or probe outcome, from the "
        "--history DIR series",
    )
    parser.add_argument(
        "--watch",
        action="store_true",
        help="scan, store the snapshot in --history DIR, then print the transitions of "
        "the window (default: the last 24 hours) instead of the hit list; exits 3 when "
        "this snapshot itself flipped a key's match",
    )
    parser.add_argument(
        "--since",
        metavar="DATE",
        help="with --changes/--trend/--watch: ignore snapshots older than DATE "
        "(YYYY-MM-DD, an ISO timestamp, or a span like 24h or 7d)",
    )
    parser.add_argument(
        "--until",
        metavar="DATE",
        help="with --changes/--trend/--watch: ignore snapshots newer than DATE",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.watch:
        if not args.history:
            print(
                "scan: --watch needs --history DIR to keep the series it reports on",
                file=sys.stderr,
            )
            return 2
        if args.trend or args.changes or args.diff:
            print(
                "scan: --watch scans and reports; drop --trend/--changes/--diff",
                file=sys.stderr,
            )
            return 2
    if args.trend:
        if not LABEL_PATTERN.fullmatch(args.trend):
            print("scan: --trend requires a numeric cid label", file=sys.stderr)
            return 2
        if not args.history:
            print("scan: --trend needs --history DIR to read the snapshot series", file=sys.stderr)
            return 2
        if args.summary or args.start or args.diff or args.changes:
            print(
                "scan: --trend only reads stored snapshots; drop "
                "--start/--summary/--diff/--changes",
                file=sys.stderr,
            )
            return 2
        return print_trend(args.history, args.trend, args.since, args.until)
    if args.changes:
        if not args.history:
            print("scan: --changes needs --history DIR to read the snapshot series", file=sys.stderr)
            return 2
        if args.summary or args.start or args.diff or args.trend:
            print(
                "scan: --changes only reads stored snapshots; drop "
                "--start/--summary/--diff/--trend",
                file=sys.stderr,
            )
            return 2
        return print_changes(args.history, args.since, args.until)
    if (args.since or args.until) and not (args.changes or args.trend or args.watch):
        print(
            "scan: --since/--until only apply to --changes, --trend or --watch",
            file=sys.stderr,
        )
        return 2
    if args.diff:
        if args.summary or args.start or args.history:
            print(
                "scan: --diff compares stored snapshots; it cannot be combined with "
                "--start, --summary or --history",
                file=sys.stderr,
            )
            return 2
        return diff_snapshots(args.diff[0], args.diff[1])
    entries = read_entries(sys.stdin)
    if not entries:
        print("scan: no content ids on stdin", file=sys.stderr)
        return 2
    if not 1 <= args.workers <= 32 or not math.isfinite(args.deadline) or not 0 < args.deadline <= 120:
        print("scan: --workers must be 1..32 and --deadline must be finite and 0..120 seconds", file=sys.stderr)
        return 2

    daemon = None
    if args.start:
        daemon = Daemon(args.start, args.workers)
        host, port = "127.0.0.1", daemon.port
    else:
        host, port = split_base(args.base_url)

    results: list[Result] = []
    pool = None
    try:
        pool = ThreadPoolExecutor(max_workers=args.workers)
        futures = {
            pool.submit(probe_stream, host, port, hexid, args.deadline): label
            for label, hexid in entries
        }
        for future, label in futures.items():
            tracks, note = future.result()
            results.append(Result(label=label, tracks=tracks, note=note))
    except KeyboardInterrupt:
        print("scan: interrupted; waiting for bounded active probes", file=sys.stderr)
        return 130
    finally:
        # Cancel queued jobs instead of waiting for the entire catalog after Ctrl-C.
        try:
            if pool is not None:
                pool.shutdown(wait=True, cancel_futures=True)
        finally:
            if daemon is not None:
                daemon.close(args.keep_workdir)

    def matches(result: Result) -> bool:
        if result.tracks is None:
            return False
        return is_aac_hit(result.tracks) if args.aac_only else is_hit(result.tracks)

    hits = sorted((result for result in results if matches(result)), key=lambda r: key_order(r.label))
    hit_labels = [result.label for result in hits]
    if not args.watch:  # a watch run prints the transitions, not the hit list
        for label in hit_labels:
            print(label)

    ordered = sorted(results, key=lambda r: key_order(r.label))
    no_audio = [r.label for r in ordered if r.tracks == []]
    no_pmt = [r.label for r in ordered if r.tracks is None]
    print(
        f"scan: scanned={len(results)} with_pmt={len(results) - len(no_pmt)} "
        f"no_audio={len(no_audio)} no_pmt={len(no_pmt)} hits={len(hits)}",
        file=sys.stderr,
    )
    if no_pmt:
        print("scan: no PMT: " + " ".join(no_pmt), file=sys.stderr)
    if no_audio:
        print("scan: no audio: " + " ".join(no_audio), file=sys.stderr)
    if not args.start and len(no_pmt) * 2 > len(results):
        print(
            "scan: hint: most streams produced no PMT; an existing daemon with the default "
            "start buffer delays the first byte. Retry with --start target/debug/outpace.",
            file=sys.stderr,
        )
    stored = None
    if args.summary or args.history:
        mode = "aac" if args.aac_only else "any"
        payload = summary_payload(results, hit_labels, mode)
        if args.summary:
            previous = load_summary(args.summary)
            if previous is not None:
                if previous.get("mode") != mode:
                    print(
                        f"scan: delta: previous snapshot used mode={previous.get('mode')}, "
                        f"this run uses mode={mode}; hit changes may reflect the criterion",
                        file=sys.stderr,
                    )
                report_delta(previous, hit_labels, {r.label: r.note for r in results})
            write_summary(args.summary, payload)
            print(f"scan: summary written to {args.summary}", file=sys.stderr)
        if args.history:
            stored_path = write_history(args.history, payload)
            stored = snapshot_base(stored_path)
            print(f"scan: history snapshot {stored_path}", file=sys.stderr)
    if args.details:
        for result in ordered:
            if result.tracks is None:
                print(f"scan: {result.label} -> {result.note}", file=sys.stderr)
            else:
                observed = describe(result.tracks) or "no audio tracks"
                print(f"scan: {result.label} = {observed}", file=sys.stderr)
    if args.watch:
        return print_changes(args.history, args.since or "24h", args.until, stored)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, OverflowError):
        print("scan: failed to read/write scan data or start isolated daemon", file=sys.stderr)
        sys.exit(2)

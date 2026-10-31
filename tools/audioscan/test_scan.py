"""Offline tests for the audio-language scanner's PAT/PMT reader and hit rules."""

from __future__ import annotations

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
import unittest.mock
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import scan  # noqa: E402  (path established above)

VIDEO = 0x100
AUDIO_1 = 0x101
AUDIO_2 = 0x102


def psi_section(table_id: int, body: bytes) -> bytes:
    length = len(body) + 4  # section header tail + CRC
    section = bytes([table_id, 0xB0 | ((length >> 8) & 0x0F), length & 0xFF]) + body
    return section + scan.mpeg_crc32(section).to_bytes(4, "big")


def pat_section(pmt_pid: int, program: int = 1) -> bytes:
    body = b"\x00\x01\xc1\x00\x00" + bytes(
        [program >> 8, program & 0xFF, 0xE0 | (pmt_pid >> 8), pmt_pid & 0xFF]
    )
    return psi_section(0x00, body)


def pmt_section(pcr_pid: int, streams: list[tuple[int, int, bytes]]) -> bytes:
    body = (
        b"\x00\x01\xc1\x00\x00"
        + bytes([0xE0 | (pcr_pid >> 8), pcr_pid & 0xFF])
        + b"\xf0\x00"
    )
    for stream_type, pid, descriptors in streams:
        body += (
            bytes([stream_type, 0xE0 | (pid >> 8), pid & 0xFF])
            + bytes([0xF0 | (len(descriptors) >> 8), len(descriptors) & 0xFF])
            + descriptors
        )
    return psi_section(0x02, body)


def language(code: bytes) -> bytes:
    return bytes([0x0A, 4]) + code + b"\x00"


def private_audio(tag: int) -> bytes:
    return bytes([tag, 1, 0x00])


def packet(pid: int, payload: bytes, pusi: bool = True, cc: int = 0) -> bytes:
    """Build a TS packet whose payload is exactly ``payload``.

    A short payload is padded with an adaptation field, never with 0xFF: trailing bytes
    inside a payload would be read as the continuation of a section that does not
    continue, which a real muxer cannot produce.
    """
    body = (b"\x00" if pusi else b"") + payload
    frame = bytearray(b"\x00" * scan.TS_PACKET)
    frame[0] = 0x47
    frame[1] = (0x40 if pusi else 0x00) | ((pid >> 8) & 0x1F)
    frame[2] = pid & 0xFF
    free = 184 - len(body)
    if free == 0:
        frame[3] = 0x10  # payload only
        start = 4
    else:
        frame[3] = 0x30  # adaptation field, then the payload
        frame[4] = free - 1
        if free > 1:
            frame[5] = 0x00  # adaptation flags; the remaining adaptation bytes stay zero
        start = 4 + free
    frame[3] |= cc & 15
    frame[start : start + len(body)] = body
    return bytes(frame)


def run(probe: scan.Probe, chunks: list[bytes]) -> scan.Probe:
    for chunk in chunks:
        probe.push(chunk)
    return probe


class ProbeTests(unittest.TestCase):
    def test_two_aac_languages_is_a_hit(self) -> None:
        probe = run(
            scan.Probe(),
            [
                packet(0x0000, pat_section(0x1000)),
                packet(
                    0x1000,
                    pmt_section(
                        VIDEO,
                        [
                            (0x1B, VIDEO, b""),
                            (0x0F, AUDIO_1, language(b"eng")),
                            (0x0F, AUDIO_2, language(b"rus")),
                        ],
                    ),
                ),
            ],
        )
        self.assertTrue(probe.done)
        self.assertEqual(
            [(track.codec, track.language, track.pid) for track in probe.tracks or []],
            [("aac", "eng", AUDIO_1), ("aac", "rus", AUDIO_2)],
        )
        self.assertTrue(scan.is_hit(probe.tracks or []))
        self.assertTrue(scan.is_aac_hit(probe.tracks or []))

    def test_same_language_is_not_a_hit(self) -> None:
        tracks = [
            scan.Track("aac", "eng", AUDIO_1),
            scan.Track("aac", "eng", AUDIO_2),
        ]
        self.assertFalse(scan.is_hit(tracks))
        self.assertFalse(scan.is_aac_hit(tracks))

    def test_undeclared_language_is_not_distinct(self) -> None:
        tracks = [
            scan.Track("aac", None, AUDIO_1),
            scan.Track("aac", "rus", AUDIO_2),
        ]
        self.assertFalse(scan.is_hit(tracks))

    def test_private_stream_type_audio_uses_codec_descriptor(self) -> None:
        probe = run(
            scan.Probe(),
            [
                packet(0x0000, pat_section(0x1000)),
                packet(
                    0x1000,
                    pmt_section(
                        VIDEO,
                        [
                            (0x06, AUDIO_1, language(b"eng") + private_audio(0x7A)),
                            (0x0F, AUDIO_2, language(b"rus")),
                        ],
                    ),
                ),
            ],
        )
        self.assertEqual([track.codec for track in probe.tracks or []], ["eac3", "aac"])
        self.assertTrue(scan.is_hit(probe.tracks or []))
        # Only one AAC track, so the engine cannot switch between these two.
        self.assertFalse(scan.is_aac_hit(probe.tracks or []))

    def test_video_only_pmt_completes_without_audio(self) -> None:
        probe = run(
            scan.Probe(),
            [
                packet(0x0000, pat_section(0x1000)),
                packet(0x1000, pmt_section(VIDEO, [(0x1B, VIDEO, b"")])),
            ],
        )
        self.assertTrue(probe.done)
        self.assertEqual(probe.tracks, [])

    def test_pmt_split_across_packets_is_reassembled(self) -> None:
        section = pmt_section(
            VIDEO,
            [
                (0x1B, VIDEO, b""),
                (0x0F, AUDIO_1, language(b"deu")),
                (0x0F, AUDIO_2, language(b"fra")),
            ],
        )
        head, tail = section[:20], section[20:]
        probe = scan.Probe()
        probe.push(packet(0x0000, pat_section(0x1000)))
        probe.push(packet(0x1000, head))
        self.assertIsNone(probe.tracks)
        probe.push(packet(0x1000, tail, pusi=False, cc=1))
        self.assertEqual(scan.declared_languages(probe.tracks or []), {"deu", "fra"})

    def test_no_pmt_leaves_tracks_unset(self) -> None:
        probe = run(scan.Probe(), [packet(0x0000, pat_section(0x1000))])
        self.assertFalse(probe.done)
        self.assertIsNone(probe.tracks)


class InputTests(unittest.TestCase):
    def test_labels_are_parsed_and_bad_lines_skipped(self) -> None:
        placeholder = "0123456789abcdef0123456789abcdef01234567"
        text = (
            "# comment\n"
            f"cid1 = {placeholder}\n"
            f"cid2\t{placeholder.upper()}\n"
            f"{placeholder}\n"
            "cid4 = not-an-id\n"
        )
        entries = scan.read_entries(io.StringIO(text))
        self.assertEqual(
            [label for label, _ in entries], ["cid1", "cid2", "#3"]
        )
        self.assertTrue(all(len(value) == 40 for _, value in entries))

    def test_non_loopback_base_is_refused(self) -> None:
        with self.assertRaises(SystemExit):
            scan.split_base("http://192.0.2.10:8621")
        self.assertEqual(scan.split_base("http://127.0.0.1:8621"), ("127.0.0.1", 8621))

    def test_key_order_is_numeric(self) -> None:
        labels = ["cid10", "cid2", "cid1"]
        self.assertEqual(sorted(labels, key=scan.key_order), ["cid1", "cid2", "cid10"])


class SummaryTests(unittest.TestCase):
    def results(self, hits: list[str], skipped: str = "cid3") -> list[scan.Result]:
        streams = {
            "cid1": [scan.Track("aac", "eng", 1), scan.Track("aac", "rus", 2)],
            "cid2": [scan.Track("aac", "deu", 3), scan.Track("aac", "fra", 4)],
        }
        results = [scan.Result(label, streams[label], "ok") for label in hits]
        results.append(scan.Result(skipped, None, "no-pmt"))
        return results

    def snapshot(self, directory: str, results: list[scan.Result]) -> str:
        path = os.path.join(directory, "summary.json")
        hits = [result.label for result in results if result.tracks and scan.is_hit(result.tracks)]
        scan.write_summary(path, scan.summary_payload(results, hits, "any"))
        return path

    def test_snapshot_reports_appeared_and_disappeared_keys(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = self.snapshot(directory, self.results(["cid1"]))
            previous = scan.load_summary(path)
            self.assertIsNotNone(previous)
            current = self.results(["cid2"])
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                scan.report_delta(previous or {}, ["cid2"], {r.label: r.note for r in current})
            report = stderr.getvalue()
            self.assertIn("appeared=cid2", report)
            self.assertIn("disappeared=cid1", report)

    def test_unchanged_run_says_so(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = self.snapshot(directory, self.results(["cid1"]))
            previous = scan.load_summary(path) or {}
            current = self.results(["cid1"])
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                scan.report_delta(previous, ["cid1"], {r.label: r.note for r in current})
            self.assertIn("no change", stderr.getvalue())

    def test_probe_status_change_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = self.snapshot(directory, self.results(["cid1"]))
            previous = scan.load_summary(path) or {}
            current = [scan.Result("cid1", None, "no-pmt")]
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                scan.report_delta(previous, [], {r.label: r.note for r in current})
            self.assertIn("cid1:ok->no-pmt", stderr.getvalue())

    def test_status_only_change_omits_empty_hit_line(self) -> None:
        previous = {
            "schema_version": 1,
            "mode": "any",
            "hits": [],
            "streams": {"cid4": {"note": "no-pmt"}},
        }
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            scan.report_delta(previous, [], {"cid4": "ok"})
        report = stderr.getvalue()
        self.assertNotIn("appeared=", report)
        self.assertIn("cid4:no-pmt->ok", report)

    def test_diff_compares_any_two_snapshots(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            before = self.snapshot(directory, self.results(["cid1"]))
            after = os.path.join(directory, "after.json")
            current = self.results(["cid2"])
            scan.write_summary(
                after,
                scan.summary_payload(
                    current,
                    [r.label for r in current if r.tracks and scan.is_hit(r.tracks)],
                    "any",
                ),
            )
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                self.assertEqual(scan.diff_snapshots(before, after), 0)
            report = stderr.getvalue()
            self.assertIn(f"{before} -> {after}", report)
            self.assertIn("appeared=cid2", report)
            self.assertIn("disappeared=cid1", report)

    def test_diff_flags_keys_present_in_only_one_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            before = self.snapshot(directory, self.results(["cid1"]))
            after = os.path.join(directory, "after.json")
            extended = self.results(["cid1"]) + [scan.Result("cid9", [scan.Track("ac3", "pol", 9)], "ok")]
            scan.write_summary(after, scan.summary_payload(extended, ["cid1"], "any"))
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                scan.diff_snapshots(before, after)
            self.assertIn("only in AFTER: cid9", stderr.getvalue())

    def test_diff_warns_when_the_criterion_changed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            before = self.snapshot(directory, self.results(["cid1"]))
            after = os.path.join(directory, "after.json")
            scan.write_summary(after, scan.summary_payload(self.results(["cid1"]), ["cid1"], "aac"))
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                scan.diff_snapshots(before, after)
            self.assertIn("mode changed any->aac", stderr.getvalue())

    def test_diff_missing_snapshot_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            present = self.snapshot(directory, self.results(["cid1"]))
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                self.assertEqual(scan.diff_snapshots(present, present + ".missing"), 2)
            self.assertIn("no snapshot at", stderr.getvalue())

    def test_diff_cannot_be_combined_with_a_scan(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(scan.main(["--diff", "a.json", "b.json", "--start", "outpace"]), 2)
        self.assertIn("cannot be combined", stderr.getvalue())

    def test_unknown_schema_is_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "summary.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump({"schema_version": 99, "hits": ["cid1"]}, handle)
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                self.assertIsNone(scan.load_summary(path))
            self.assertIn("unknown schema", stderr.getvalue())

    def test_missing_snapshot_is_not_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            self.assertIsNone(scan.load_summary(os.path.join(directory, "absent.json")))

    @unittest.skipUnless(os.name == "posix", "POSIX file permissions")
    def test_snapshot_is_private(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = self.snapshot(directory, self.results(["cid1"]))
            self.assertEqual(os.stat(path).st_mode & 0o777, 0o600)

    def test_snapshot_carries_labels_not_identifiers(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = self.snapshot(directory, self.results(["cid1"]))
            with open(path, "r", encoding="utf-8") as handle:
                text = handle.read()
            self.assertNotRegex(text, r"[0-9a-f]{40}")
            self.assertEqual(json.loads(text)["hits"], ["cid1"])


class HistoryTests(unittest.TestCase):
    def payload(self, labels: list[str], hits: list[str], mode: str = "any", note: str = "ok"):
        results = [
            scan.Result(label, [scan.Track("aac", "eng", 1), scan.Track("ac3", "pol", 2)], note)
            for label in labels
        ]
        return scan.summary_payload(results, hits, mode)

    def stamp(self, day: int, hour: int = 2, minute: int = 27, second: int = 51) -> datetime:
        return datetime(2026, 10, day, hour, minute, second, tzinfo=timezone.utc)

    def test_history_names_carry_the_date_and_never_collide(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            first = scan.write_history(directory, self.payload(["cid1"], []), self.stamp(28))
            second = scan.write_history(directory, self.payload(["cid1"], []), self.stamp(28))
            self.assertEqual(os.path.basename(first), "20261028T022751Z.json")
            self.assertEqual(os.path.basename(second), "20261028T022751Z-2.json")

    def test_history_reads_oldest_first_and_skips_foreign_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(30))
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(28))
            with open(os.path.join(directory, "notes.json"), "w", encoding="utf-8") as handle:
                handle.write("not a snapshot")
            with open(
                os.path.join(directory, "20261029T000000Z.json"), "w", encoding="utf-8"
            ) as handle:
                json.dump({"schema_version": 99}, handle)
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                series = scan.read_history(directory)
            self.assertEqual([base for base, _ in series], ["20261028T022751Z", "20261030T022751Z"])
            self.assertIn("unknown schema", stderr.getvalue())

    def test_trend_reports_each_snapshot_for_one_key(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1", "cid2"], ["cid1"]), self.stamp(28))
            scan.write_history(directory, self.payload(["cid1", "cid2"], []), self.stamp(29))
            scan.write_history(directory, self.payload(["cid2"], []), self.stamp(30))
            stdout, stderr = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                self.assertEqual(scan.print_trend(directory, "cid1"), 0)
            rows = stdout.getvalue().strip().splitlines()
            self.assertEqual(len(rows), 4)  # header and three snapshots
            self.assertIn("yes", rows[1])
            self.assertIn("no", rows[2])
            self.assertIn("absent", rows[3])
            report = stderr.getvalue()
            self.assertIn("3 snapshots, matched in 1, absent from 1", report)
            self.assertIn("last change 2026-10-30T02:27:51Z", report)

    def test_trend_counts_a_probe_flip_as_a_change(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(28))
            scan.write_history(
                directory, self.payload(["cid1"], [], note="no-pmt"), self.stamp(29)
            )
            stderr = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                scan.print_trend(directory, "cid1")
            self.assertIn("last change 2026-10-29T02:27:51Z", stderr.getvalue())

    def test_trend_warns_when_the_history_spans_modes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], ["cid1"]), self.stamp(28))
            scan.write_history(
                directory, self.payload(["cid1"], [], mode="aac"), self.stamp(29)
            )
            stderr = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                scan.print_trend(directory, "cid1")
            self.assertIn("span modes aac,any", stderr.getvalue())

    def test_trend_says_when_a_key_was_never_scanned(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(28))
            stderr = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                scan.print_trend(directory, "cid9")
            self.assertIn("never appears in this history", stderr.getvalue())

    def test_trend_missing_directory_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                self.assertEqual(scan.print_trend(os.path.join(directory, "absent"), "cid1"), 2)
            self.assertIn("no history directory", stderr.getvalue())

    def test_changes_lists_match_and_probe_transitions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1", "cid2"], ["cid1"]), self.stamp(28))
            scan.write_history(directory, self.payload(["cid1", "cid2"], []), self.stamp(29))
            scan.write_history(
                directory,
                self.payload(["cid1", "cid2"], [], note="no-pmt"),
                self.stamp(30),
            )
            stdout, stderr = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                self.assertEqual(scan.print_changes(directory, None, None), 0)
            rows = stdout.getvalue().strip().splitlines()
            self.assertEqual(len(rows), 3)
            self.assertIn("2026-10-29T02:27:51Z  cid1      match  yes       no", rows[0])
            self.assertIn("2026-10-30T02:27:51Z  cid1      probe  ok        no-pmt", rows[1])
            self.assertIn("2026-10-30T02:27:51Z  cid2      probe  ok        no-pmt", rows[2])
            self.assertIn("3 transitions across 2 keys in 3 snapshots", stderr.getvalue())

    def test_changes_window_filters_snapshots(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], ["cid1"]), self.stamp(28))
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(29))
            stdout, stderr = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                self.assertEqual(scan.print_changes(directory, "2026-10-29", None), 0)
            self.assertEqual(stdout.getvalue(), "")
            self.assertIn("no transitions in 1 snapshot", stderr.getvalue())

    def test_changes_reports_keys_missing_from_some_snapshots(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1", "cid2"], []), self.stamp(28))
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(29))
            stdout, stderr = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                scan.print_changes(directory, None, None)
            self.assertIn("gaps are coverage", stderr.getvalue())
            self.assertIn("cid2", stderr.getvalue())

    def test_a_fresh_match_flip_exits_three(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(28))
            scan.write_history(directory, self.payload(["cid1"], ["cid1"]), self.stamp(29))
            stderr = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                self.assertEqual(
                    scan.print_changes(directory, None, None, "20261029T022751Z"), 3
                )
            report = stderr.getvalue()
            self.assertIn("new match transition in this snapshot (cid1 no->yes)", report)
            self.assertIn("exiting 3", report)

    def test_an_older_match_flip_does_not_refire(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(28))
            scan.write_history(directory, self.payload(["cid1"], ["cid1"]), self.stamp(29))
            scan.write_history(directory, self.payload(["cid1"], ["cid1"]), self.stamp(30))
            with contextlib.redirect_stdout(
                io.StringIO()
            ), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(
                    scan.print_changes(directory, None, None, "20261030T022751Z"), 0
                )

    def test_a_fresh_probe_flip_alone_exits_zero(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(28))
            scan.write_history(
                directory, self.payload(["cid1"], [], note="no-pmt"), self.stamp(29)
            )
            with contextlib.redirect_stdout(
                io.StringIO()
            ), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(
                    scan.print_changes(directory, None, None, "20261029T022751Z"), 0
                )

    def test_snapshot_base_reads_written_names(self) -> None:
        self.assertEqual(scan.snapshot_base("/tmp/20261030T022751Z.json"), "20261030T022751Z")
        self.assertEqual(scan.snapshot_base("20261030T022751Z-2.json"), "20261030T022751Z-2")
        self.assertIsNone(scan.snapshot_base("/tmp/notes.json"))

    def test_changes_rejects_an_unreadable_date(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(28))
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                self.assertEqual(scan.print_changes(directory, "31.10.2026", None), 2)
            self.assertIn("cannot read date", stderr.getvalue())

    def test_changes_rejects_a_reversed_window(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(28))
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                self.assertEqual(
                    scan.print_changes(directory, "2026-10-30", "2026-10-28"), 2
                )
            self.assertIn("--since is after --until", stderr.getvalue())

    def test_relative_bounds_are_anchored_to_now(self) -> None:
        anchor = datetime(2026, 10, 30, 12, 0, 0, tzinfo=timezone.utc)
        self.assertEqual(scan.parse_bound("24h", now=anchor), "20261029T120000Z")
        self.assertEqual(scan.parse_bound("7d", now=anchor), "20261023T120000Z")
        self.assertEqual(scan.parse_bound("2026-10-30", now=anchor), "20261030T000000Z")

    def test_watch_needs_a_history_directory(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(scan.main(["--watch"]), 2)
        self.assertIn("needs --history", stderr.getvalue())

    def test_watch_cannot_be_combined_with_read_only_modes(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(scan.main(["--watch", "--history", "d", "--changes"]), 2)
        self.assertIn("scans and reports", stderr.getvalue())

    def test_window_flags_are_allowed_for_watch(self) -> None:
        # A window on a watch run is accepted; the run then fails on the empty stdin
        # instead of on the flag, which is what this asserts.
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), unittest.mock.patch("sys.stdin", io.StringIO("")):
            self.assertEqual(scan.main(["--watch", "--history", "/tmp", "--since", "7d"]), 2)
        self.assertIn("no content ids on stdin", stderr.getvalue())

    def test_changes_needs_a_history_directory(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(scan.main(["--changes"]), 2)
        self.assertIn("needs --history", stderr.getvalue())

    def test_window_flags_need_a_report_mode(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(scan.main(["--since", "2026-10-28"]), 2)
        self.assertIn("only apply to --changes, --trend or --watch", stderr.getvalue())

    def test_changes_cannot_be_combined_with_a_scan(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(
                scan.main(["--changes", "--history", "d", "--start", "x"]), 2
            )
        self.assertIn("only reads stored snapshots", stderr.getvalue())

    def test_trend_honours_the_window(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            scan.write_history(directory, self.payload(["cid1"], ["cid1"]), self.stamp(28))
            scan.write_history(directory, self.payload(["cid1"], []), self.stamp(29))
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(io.StringIO()):
                scan.print_trend(directory, "cid1", "2026-10-29", None)
            rows = stdout.getvalue().strip().splitlines()
            self.assertEqual(len(rows), 2)  # header and one snapshot
            self.assertIn("2026-10-29", rows[1])

    def test_trend_requires_a_history_directory(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(scan.main(["--trend", "cid1"]), 2)
        self.assertIn("needs --history", stderr.getvalue())

    def test_trend_cannot_be_combined_with_a_scan(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(scan.main(["--trend", "cid1", "--history", "d", "--start", "x"]), 2)
        self.assertIn("only reads stored snapshots", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()

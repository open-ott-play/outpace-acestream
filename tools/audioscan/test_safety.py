"""Offline PSI regressions, lifecycle fault injection, and local HTTP deadlines."""
import contextlib
import io
import json
import os
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch

import scan
from test_scan import language, packet, pat_section, pmt_section

SYNTHETIC_ID = "0123456789abcdef0123456789abcdef01234567"


def table():
    return pmt_section(256, [(15, 257, language(b"eng")), (15, 258, language(b"rus"))])


def crc(section):
    return bytes(section[:-4]) + scan.mpeg_crc32(section[:-4]).to_bytes(4, "big")


def ready_probe():
    probe = scan.Probe()
    probe.push(packet(0, pat_section(4096)))
    return probe


class PsiSafety(unittest.TestCase):
    def test_crc_and_noncurrent_are_rejected_then_valid_table_accepted(self):
        for offset, bit in [(5, 1), (-1, 1)]:
            with self.subTest(offset=offset):
                probe = ready_probe()
                bad = bytearray(table())
                bad[offset] ^= bit
                if offset == 5:
                    bad = crc(bad)
                probe.push(packet(4096, bad))
                self.assertFalse(probe.done)
                probe.push(packet(4096, table(), cc=1))
                self.assertTrue(probe.done)

    def test_partial_section_after_gap_not_accepted(self):
        probe = ready_probe()
        source = table()
        probe.push(packet(4096, source[:20]))
        probe.push(packet(4096, source[20:], pusi=False, cc=2))
        self.assertFalse(probe.done)
        probe.push(packet(4096, source, cc=3))
        self.assertTrue(probe.done)

    def test_duplicate_packet_does_not_damage_assembly(self):
        probe = ready_probe()
        source = table()
        first = packet(4096, source[:20])
        probe.push(first)
        probe.push(first)
        probe.push(packet(4096, source[20:], pusi=False, cc=1))
        self.assertTrue(probe.done)

    def test_pointer_tail_completes_previous_section(self):
        assembler = scan.Sections()
        source = table()
        self.assertEqual(assembler.feed(b"\0" + source[:20], True), [])
        tail = source[20:]
        self.assertEqual(assembler.feed(bytes([len(tail)]) + tail + source, True), [source, source])

    def test_bad_descriptor_and_section_lengths_are_rejected(self):
        for modification in ["es", "program", "section"]:
            probe = ready_probe()
            bad = bytearray(table())
            if modification == "es":
                bad[16] = 250
            elif modification == "program":
                bad[11] = 250
            else:
                bad[2] -= 1
            probe._on_pmt(crc(bad))
            self.assertFalse(probe.done)

    def test_lowest_pat_program_selected_independent_of_order(self):
        def pat(entries):
            body = bytearray(b"\x00\x01\xc1\x00\x00")
            for program, pid in entries:
                body.extend([program >> 8, program & 255, 0xE0 | (pid >> 8), pid & 255])
            from test_scan import psi_section
            return psi_section(0, bytes(body))
        for entries in [[(9, 4097), (1, 4096)], [(1, 4096), (9, 4097)]]:
            probe = scan.Probe()
            probe.push(packet(0, pat(entries)))
            self.assertEqual(probe.program, (1, 4096))
            probe.push(packet(4097, table()))
            self.assertFalse(probe.done)
            probe.push(packet(4096, table()))
            self.assertTrue(probe.done)

    def test_wrong_program_and_multisection_pat_are_not_guessed(self):
        probe = ready_probe()
        bad = bytearray(table())
        bad[4] = 2
        probe._on_pmt(crc(bad))
        self.assertFalse(probe.done)
        bad = bytearray(pat_section(4096))
        bad[7] = 1
        probe._on_pat(crc(bad))
        self.assertEqual(probe.note, "unsupported-pat-sections")
        self.assertIsNone(probe.program)

    def test_tei_scramble_and_discontinuity_discard_partial_section(self):
        for mode in ["tei", "scramble", "discontinuity"]:
            with self.subTest(mode=mode):
                probe = ready_probe()
                source = table()
                probe.push(packet(4096, source[:20]))
                tail = bytearray(packet(4096, source[20:], pusi=False, cc=1))
                if mode == "tei":
                    tail[1] |= 0x80
                elif mode == "scramble":
                    tail[3] |= 0x80
                else:
                    tail[5] |= 0x80
                probe.push(bytes(tail))
                self.assertFalse(probe.done)

    def test_latm_is_inventory_but_not_native_adts_candidate(self):
        tracks = [scan.Track("aac-latm", "eng", 257), scan.Track("aac-latm", "rus", 258)]
        self.assertTrue(scan.is_hit(tracks))
        self.assertFalse(scan.is_aac_hit(tracks))

    def test_conflicting_codec_is_rejected(self):
        probe = ready_probe()
        probe._on_pmt(pmt_section(256, [(15, 257, language(b"eng") + bytes([0x6A, 1, 0]))]))
        self.assertFalse(probe.done)

    def test_long_invalid_stream_does_not_accumulate(self):
        probe = ready_probe()
        for _ in range(20):
            probe.push(b"x" * 65536)
            self.assertLess(len(probe.buffer), 188)


class HttpSafety(unittest.TestCase):
    def serve_once(self, writer):
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(3)
        port = listener.getsockname()[1]
        observed = []
        errors = []
        def serve():
            try:
                conn, _ = listener.accept()
                with conn:
                    conn.settimeout(2)
                    observed.append(conn.recv(4096))
                    writer(conn)
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                # Deadline tests intentionally close the peer during writes; the test
                # still asserts its observed request and bounded client completion.
                pass
            except BaseException as error:
                # Thread exceptions do not otherwise fail unittest's owning test.
                errors.append(error)
            finally:
                listener.close()
        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        def finish():
            worker.join(3)
            self.assertFalse(worker.is_alive(), "HTTP test worker did not stop")
            if errors:
                raise errors[0]
        self.addCleanup(finish)
        return port, observed

    def test_existing_daemon_is_get_only_even_on_success(self):
        body = packet(0, pat_section(4096)) + packet(4096, table())
        port, observed = self.serve_once(lambda conn: conn.sendall(
            b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body))
        with patch.object(scan.sys, "stdin", io.StringIO("cid1=" + SYNTHETIC_ID)), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(scan.main(["--base-url", f"http://127.0.0.1:{port}"]), 0)
        self.assertEqual(len(observed), 1)
        self.assertTrue(observed[0].startswith(b"GET "))
        self.assertFalse(hasattr(scan, "stop_stream"))

    def test_slow_header_and_body_are_bounded_by_total_deadline(self):
        for headers in [True, False]:
            with self.subTest(headers=headers):
                def slow(conn):
                    if not headers:
                        conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 999999\r\n\r\n")
                    for _ in range(80):
                        conn.sendall(b"H" if headers else b"x")
                        time.sleep(0.02)
                port, _ = self.serve_once(slow)
                started = time.monotonic()
                self.assertEqual(scan.probe_stream("127.0.0.1", port, SYNTHETIC_ID, 0.15), (None, "no-pmt"))
                self.assertLess(time.monotonic() - started, 0.8)

    def test_unexpected_server_failure_is_surfaced_from_joined_cleanup(self):
        class BrokenServerCase(HttpSafety):
            def runTest(self):
                def fail(conn):
                    raise OSError("injected unexpected HTTP server failure")
                port, _ = self.serve_once(fail)
                scan.probe_stream("127.0.0.1", port, SYNTHETIC_ID, 0.2)
        result = unittest.TestResult()
        BrokenServerCase().run(result)
        self.assertEqual(result.testsRun, 1)
        self.assertEqual(len(result.failures), 0)
        self.assertEqual(len(result.errors), 1)
        self.assertIn("OSError: injected unexpected HTTP server failure", result.errors[0][1])

    def test_base_rejects_credentials_paths_and_remote_hosts(self):
        for base in ["http://user:secret@127.0.0.1", "http://127.0.0.1/foo", "http://127.0.0.1?token=secret", "http://example.com", "http://127.0.0.1:bad"]:
            with self.subTest(base=base), self.assertRaises(SystemExit):
                scan.split_base(base)


class LifecycleSafety(unittest.TestCase):
    def test_readiness_failure_terminates_and_reaps_started_child(self):
        proc = Mock()
        proc.poll.return_value = None
        with tempfile.TemporaryDirectory() as parent:
            workdir = os.path.join(parent, "run")
            os.mkdir(workdir)
            with patch.object(scan.tempfile, "mkdtemp", return_value=workdir), patch.object(scan, "free_port", return_value=18444), patch.object(scan.subprocess, "Popen", return_value=proc), patch.object(scan.Daemon, "_await_ready", side_effect=SystemExit("injected")):
                with self.assertRaises(SystemExit):
                    scan.Daemon("unused", 1)
            proc.terminate.assert_called_once()
            proc.wait.assert_called_once_with(timeout=10)
            self.assertFalse(os.path.exists(workdir))

    def test_stubborn_child_killed_and_reaped(self):
        daemon = object.__new__(scan.Daemon)
        with tempfile.TemporaryDirectory() as directory:
            daemon.workdir = directory
            daemon.proc = Mock()
            daemon.proc.poll.return_value = None
            daemon.proc.wait.side_effect = [subprocess.TimeoutExpired("synthetic", 10), None]
            daemon.close(False)
            daemon.proc.kill.assert_called_once()
            self.assertEqual(daemon.proc.wait.call_count, 2)

    def test_launch_failure_removes_private_directory(self):
        with tempfile.TemporaryDirectory() as parent:
            workdir = os.path.join(parent, "run")
            os.mkdir(workdir)
            with patch.object(scan.tempfile, "mkdtemp", return_value=workdir), patch.object(scan, "free_port", return_value=18444), patch.object(scan.subprocess, "Popen", side_effect=OSError("injected")):
                with self.assertRaises(OSError):
                    scan.Daemon("unused", 1)
            self.assertFalse(os.path.exists(workdir))

    def test_unsafe_labels_never_enter_output_or_snapshots(self):
        with contextlib.redirect_stderr(io.StringIO()) as err:
            self.assertEqual(scan.read_entries(io.StringIO("bad-label=" + SYNTHETIC_ID)), [])
        self.assertNotIn(SYNTHETIC_ID, err.getvalue())
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "snapshot.json")
            with open(path, "w") as output:
                json.dump({"schema_version": 1, "mode": "any", "hits": [SYNTHETIC_ID], "streams": {SYNTHETIC_ID: {"note": "ok", "tracks": []}}}, output)
            with contextlib.redirect_stderr(io.StringIO()) as err:
                self.assertIsNone(scan.load_summary(path))
            self.assertNotIn(SYNTHETIC_ID, err.getvalue())

    def test_malformed_snapshot_codec_and_depth_are_ignored(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "snapshot.json")
            for codec in [[], {}]:
                document = {"schema_version": 1, "mode": "any", "hits": [], "streams": {"cid1": {"note": "ok", "tracks": [{"codec": codec, "pid": 257, "language": "eng"}]}}}
                with open(path, "w") as output:
                    json.dump(document, output)
                with contextlib.redirect_stderr(io.StringIO()):
                    self.assertIsNone(scan.load_summary(path))
            with open(path, "w") as output:
                output.write("[" * 2000 + "]" * 2000)
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertIsNone(scan.load_summary(path))

    def test_interrupted_pool_shutdown_still_closes_private_child(self):
        from concurrent.futures import Future
        completed = Future()
        completed.set_result(([], "ok"))
        pool = Mock()
        pool.submit.return_value = completed
        pool.shutdown.side_effect = KeyboardInterrupt()
        daemon = Mock(port=18444)
        with patch.object(scan, "ThreadPoolExecutor", return_value=pool), patch.object(scan, "Daemon", return_value=daemon), patch.object(scan.sys, "stdin", io.StringIO("cid1=" + SYNTHETIC_ID)):
            with self.assertRaises(KeyboardInterrupt):
                scan.main(["--start", "unused"])
        daemon.close.assert_called_once_with(False)

    def test_private_config_does_not_scale_peer_fanout_with_workers(self):
        proc = Mock()
        proc.poll.return_value = None
        with patch.dict(os.environ, {"OUTPACE_MAX_ACTIVE_UPSTREAMS": "99", "OUTPACE_BIND": "0.0.0.0:9999"}), patch.object(scan.subprocess, "Popen", return_value=proc) as popen, patch.object(scan.Daemon, "_await_ready"):
            daemon = scan.Daemon("unused", 32)
            try:
                environment = popen.call_args.kwargs["env"]
                self.assertNotIn("OUTPACE_MAX_ACTIVE_UPSTREAMS", environment)
                self.assertTrue(environment["OUTPACE_BIND"].startswith("127.0.0.1:"))
            finally:
                daemon.close(False)

    def test_actual_child_reaped_when_startup_fails(self):
        import sys
        real_popen = subprocess.Popen
        children = []
        def launch(*args, **kwargs):
            child = real_popen([sys.executable, "-c", "import time; time.sleep(60)"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            children.append(child)
            return child
        try:
            with patch.object(scan.subprocess, "Popen", side_effect=launch), patch.object(scan.Daemon, "_await_ready", side_effect=SystemExit("injected readiness failure")):
                with self.assertRaises(SystemExit):
                    scan.Daemon("unused", 1)
            self.assertEqual(len(children), 1)
            self.assertIsNotNone(children[0].poll())
        finally:
            for child in children:
                if child.poll() is None:
                    child.kill()
                    child.wait()

    def test_nonfinite_deadlines_and_excess_workers_rejected(self):
        for args in [["--deadline", "nan"], ["--deadline", "inf"], ["--workers", "33"]]:
            with patch.object(scan.sys, "stdin", io.StringIO("cid1=" + SYNTHETIC_ID)), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(scan.main(args), 2)

class HistorySafety(unittest.TestCase):
    def test_same_second_watch_does_not_refire_older_change(self):
        from datetime import datetime, timezone
        moment = datetime(2026, 10, 31, 0, 0, 0, tzinfo=timezone.utc)
        observation = [scan.Result("cid1", [scan.Track("aac", "eng", 257)], "ok")]
        with tempfile.TemporaryDirectory() as directory:
            before = scan.write_history(directory, scan.summary_payload(observation, [], "any"), moment)
            changed = scan.write_history(directory, scan.summary_payload(observation, ["cid1"], "any"), moment)
            unchanged = scan.write_history(directory, scan.summary_payload(observation, ["cid1"], "any"), moment)
            self.assertNotEqual(before, changed)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(scan.print_changes(directory, None, None, scan.snapshot_base(changed)), 3)
                self.assertEqual(scan.print_changes(directory, None, None, scan.snapshot_base(unchanged)), 0)
            self.assertTrue(scan.in_window(scan.snapshot_base(unchanged), None, "20261031T000000Z"))

    def test_concurrent_history_writes_preserve_every_snapshot(self):
        from concurrent.futures import ThreadPoolExecutor
        from datetime import datetime, timezone
        moment = datetime(2026, 10, 31, 0, 0, 0, tzinfo=timezone.utc)
        with tempfile.TemporaryDirectory() as directory:
            def save(index):
                result = [scan.Result(f"cid{index}", [], "ok")]
                return scan.write_history(directory, scan.summary_payload(result, [], "any"), moment)
            with ThreadPoolExecutor(max_workers=4) as pool:
                paths = list(pool.map(save, range(8)))
            self.assertEqual(len(set(paths)), 8)
            self.assertEqual(len(scan.read_history(directory)), 8)


if __name__ == "__main__":
    unittest.main()

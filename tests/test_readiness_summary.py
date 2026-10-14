import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools/readiness_summary.py"


def readiness(sequence, at, before, after, event, reason=0, queue=None, trace_id=None):
    record = {
        "version": 3 if trace_id is not None else 2,
        "session": 1,
        "sequence": sequence,
        "elapsed_us": at,
        "readiness_event": event,
        "reason": reason,
        "ready_before": before,
        "ready_after": after,
        "pacer_queue": queue,
        "log_dropped": 0,
        "log_failed": False,
    }
    if trace_id is not None:
        record["trace_id"] = trace_id
    return record


def run_tool(lines):
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "trace.jsonl"
        path.write_text("\n".join(lines) + "\n")
        return subprocess.run(
            [sys.executable, str(TOOL), str(path)],
            text=True,
            capture_output=True,
            check=False,
        )


class ReadinessSummaryTests(unittest.TestCase):
    def test_counts_fall_between_peer_samples_and_terminal_gap(self):
        queue = {"sample_age_ms": 4, "first_unsent_age_ms": 80, "bytes": 2048, "packets": 3}
        records = [
            readiness(1, 1_000_000, False, True, 2),
            {"version": 3, "session": 1, "elapsed_ms": 1000, "bitrate_kbps": 560,
             "input_ready": True, "terminal": False},
            readiness(2, 1_200_000, True, False, 1, 9, queue),
            readiness(3, 1_500_000, False, True, 2),
            readiness(4, 1_700_000, True, False, 1, 9),
            {
                "session": 1,
                "version": 3,
                "elapsed_ms": 2000,
                "bitrate_kbps": 560,
                "input_ready": False,
                "fault": "stopped",
                "terminal": True,
            },
        ]
        result = run_tool([json.dumps(record) for record in records])
        self.assertEqual(result.returncode, 0, result.stderr)
        session = json.loads(result.stdout)["sessions"][0]
        self.assertEqual(session["coverage"], "complete")
        self.assertEqual(session["falls"], 2)
        self.assertEqual(session["display_expired_falls"], 2)
        self.assertEqual(session["completed_unready_us"], 300_000)
        self.assertEqual(session["max_completed_gap_us"], 300_000)
        self.assertEqual(session["terminal_gap_us_range"], [300_000, 300_999])
        self.assertEqual(session["total_unready_us_range"], [600_000, 600_999])
        self.assertEqual(session["completed_gaps"][0]["pacer_queue"], queue)

    def test_missing_sequence_and_open_end_are_partial(self):
        records = [
            readiness(1, 1_000_000, False, True, 2),
            readiness(3, 1_200_000, True, False, 1, 9),
            {"version": 3, "session": 1, "elapsed_ms": 1700,
             "bitrate_kbps": 560, "terminal": False},
        ]
        result = run_tool([json.dumps(record) for record in records])
        self.assertEqual(result.returncode, 0, result.stderr)
        session = json.loads(result.stdout)["sessions"][0]
        self.assertEqual(session["coverage"], "partial")
        self.assertIn("readiness_sequence_gap", session["issues"])
        self.assertIn("session_end_not_observed", session["issues"])
        self.assertEqual(session["open_gap_observed_lower_bound_us"], 500_000)
        self.assertEqual(session["completed_unready_us"], 0)
        self.assertIsNone(session["total_unready_us_range"])

    def test_archived_numeric_trace_reproduces_reported_gaps(self):
        path = ROOT / "tests/fixtures/low-data-readiness.jsonl"
        result = subprocess.run(
            [sys.executable, str(TOOL), str(path)],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        session = json.loads(result.stdout)["sessions"][0]
        self.assertEqual(session["coverage"], "complete")
        self.assertEqual(session["falls"], 35)
        self.assertEqual(session["display_expired_falls"], 35)
        self.assertEqual(session["completed_unready_us"], 13_774_493)
        self.assertEqual(session["max_completed_gap_us"], 1_565_486)

    def test_appended_host_runs_keep_reused_session_numbers_separate(self):
        one_run = [
            readiness(1, 100_000, False, True, 2),
            readiness(2, 200_000, True, False, 1, 9),
            readiness(3, 400_000, False, True, 2),
            {"version": 3, "session": 1, "elapsed_ms": 500,
             "bitrate_kbps": 560, "fault": "stopped", "terminal": True},
        ]
        result = run_tool([json.dumps(record) for record in one_run * 2])
        self.assertEqual(result.returncode, 0, result.stderr)
        sessions = json.loads(result.stdout)["sessions"]
        self.assertEqual([session["occurrence"] for session in sessions], [1, 2])
        self.assertEqual([session["coverage"] for session in sessions], ["complete", "complete"])
        self.assertEqual([session["completed_unready_us"] for session in sessions],
                         [200_000, 200_000])

    def test_terminal_failure_ends_gap_but_reset_fault_does_not(self):
        records = [
            readiness(1, 100_000, False, True, 2),
            readiness(2, 200_000, True, False, 1, 9),
            {"version": 3, "session": 1, "elapsed_ms": 250,
             "bitrate_kbps": 560, "fault": "lock_changed", "terminal": False},
            readiness(3, 300_000, False, True, 2),
            readiness(4, 400_000, True, False, 1, 9),
            {"version": 3, "session": 1, "elapsed_ms": 600,
             "bitrate_kbps": 560, "fault": "encoder_operation", "terminal": True},
        ]
        result = run_tool([json.dumps(record) for record in records])
        self.assertEqual(result.returncode, 0, result.stderr)
        session = json.loads(result.stdout)["sessions"][0]
        self.assertEqual(session["coverage"], "complete")
        self.assertEqual(session["terminal_fault"], "encoder_operation")
        self.assertEqual(session["completed_unready_us"], 100_000)
        self.assertEqual(session["terminal_gap_us_range"], [200_000, 200_999])
        self.assertEqual(session["total_unready_us_range"], [300_000, 300_999])

    def test_dropped_restart_records_do_not_merge_trace_ids(self):
        records = [
            readiness(1, 100_000, False, True, 2, trace_id=11),
            readiness(2, 200_000, True, False, 1, 9, trace_id=11),
            readiness(2, 300_000, False, True, 2, trace_id=22),
            {"version": 4, "session": 1, "trace_id": 22, "elapsed_ms": 400,
             "bitrate_kbps": 560, "fault": "stopped", "terminal": True},
        ]
        records[2]["log_dropped"] = 2
        result = run_tool([json.dumps(record) for record in records])
        self.assertEqual(result.returncode, 0, result.stderr)
        sessions = json.loads(result.stdout)["sessions"]
        self.assertEqual([session["trace_id"] for session in sessions], [11, 22])
        self.assertEqual([session["coverage"] for session in sessions], ["partial", "partial"])
        self.assertIsNone(sessions[0]["total_unready_us_range"])
        self.assertIsNone(sessions[1]["total_unready_us_range"])

    def test_missing_fall_cannot_report_exact_zero_unready_time(self):
        records = [
            readiness(1, 100_000, False, True, 2),
            readiness(3, 300_000, False, True, 2),
            {"version": 3, "session": 1, "elapsed_ms": 400,
             "bitrate_kbps": 560, "fault": "stopped", "terminal": True},
        ]
        records[1]["log_dropped"] = 1
        result = run_tool([json.dumps(record) for record in records])
        self.assertEqual(result.returncode, 0, result.stderr)
        session = json.loads(result.stdout)["sessions"][0]
        self.assertEqual(session["coverage"], "partial")
        self.assertIn("readiness_sequence_gap", session["issues"])
        self.assertEqual(session["completed_unready_us"], 0)
        self.assertIsNone(session["total_unready_us_range"])

    def test_malformed_json_reports_source_line(self):
        result = run_tool([json.dumps(readiness(1, 100, False, True, 2)), "{"])
        self.assertEqual(result.returncode, 1)
        self.assertIn("trace.jsonl:2: invalid JSON", result.stderr)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()

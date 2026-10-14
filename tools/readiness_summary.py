#!/usr/bin/env python3
"""Summarize ScreenPier input-readiness transitions from a diagnostics log."""

import argparse
from dataclasses import dataclass, field
import json
from pathlib import Path
import sys


AMBIGUOUS_LEGACY_FAULTS = {"lock_changed", "geometry_changed", "operation"}
PEER_FALLING = 1
PEER_RISING = 2
DISPLAY_EXPIRED = 9


def integer(record: dict, key: str, source: str) -> int:
    value = record.get(key)
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{source}: {key} must be a nonnegative integer")
    return value


def boolean(record: dict, key: str, source: str) -> bool:
    value = record.get(key)
    if not isinstance(value, bool):
        raise ValueError(f"{source}: {key} must be a boolean")
    return value


def queue_snapshot(record: dict, source: str) -> dict | None:
    queue = record.get("pacer_queue")
    if queue is None:
        return None
    if not isinstance(queue, dict):
        raise ValueError(f"{source}: pacer_queue must be an object or null")
    result = {}
    for key in ("sample_age_ms", "bytes", "packets"):
        result[key] = integer(queue, key, source)
    result["first_unsent_age_ms"] = (
        None if queue.get("first_unsent_age_ms") is None
        else integer(queue, "first_unsent_age_ms", source)
    )
    return result


@dataclass
class Session:
    number: int
    occurrence: int
    trace_id: int | None = None
    first_ready_us: int | None = None
    last_readiness_us: int | None = None
    last_sequence: int | None = None
    last_observed_us: int = 0
    known_ready: bool | None = None
    open_gap: dict | None = None
    gaps: list[dict] = field(default_factory=list)
    terminal_ms: int | None = None
    terminal_fault: str | None = None
    issues: set[str] = field(default_factory=set)

    def observe_peer(self, record: dict, source: str) -> None:
        elapsed_ms = integer(record, "elapsed_ms", source)
        self.last_observed_us = max(self.last_observed_us, elapsed_ms * 1000)
        fault = record.get("fault")
        if fault is not None and not isinstance(fault, str):
            raise ValueError(f"{source}: fault must be a string or null")
        version = integer(record, "version", source)
        if version >= 4 and self.trace_id is None:
            raise ValueError(f"{source}: version 4 peer record requires trace_id")
        if version >= 3:
            terminal = boolean(record, "terminal", source)
        else:
            terminal = fault is not None and fault not in AMBIGUOUS_LEGACY_FAULTS
        if terminal:
            self.terminal_ms = elapsed_ms
            self.terminal_fault = fault

    def observe_readiness(self, record: dict, source: str) -> None:
        sequence = integer(record, "sequence", source)
        at = integer(record, "elapsed_us", source)
        if integer(record, "version", source) >= 3 and self.trace_id is None:
            raise ValueError(f"{source}: version 3 readiness record requires trace_id")
        event = integer(record, "readiness_event", source)
        before = boolean(record, "ready_before", source)
        after = boolean(record, "ready_after", source)
        if self.last_sequence is None:
            if sequence != 1:
                self.issues.add("initial_history_missing")
            if before:
                self.issues.add("initial_ready_unobserved")
        elif sequence <= self.last_sequence:
            raise ValueError(f"{source}: readiness sequence is not increasing")
        elif sequence != self.last_sequence + 1:
            self.issues.add("readiness_sequence_gap")
            self.open_gap = None
        if self.last_readiness_us is not None and at < self.last_readiness_us:
            raise ValueError(f"{source}: readiness time moved backwards")
        if event == PEER_FALLING and (not before or after):
            raise ValueError(f"{source}: PeerFalling has invalid ready states")
        if event == PEER_RISING and (before or not after):
            raise ValueError(f"{source}: PeerRising has invalid ready states")
        if self.known_ready is not None and before != self.known_ready:
            self.issues.add("readiness_state_gap")
            self.open_gap = None
        self.last_sequence = sequence
        self.last_readiness_us = at
        self.last_observed_us = max(self.last_observed_us, at)
        self.known_ready = after
        if record.get("log_dropped", 0):
            self.issues.add("diagnostics_dropped")
        if record.get("log_failed", False):
            self.issues.add("diagnostics_failed")
        if not before and after:
            if self.first_ready_us is None:
                self.first_ready_us = at
            if self.open_gap is not None:
                self.open_gap["end_us"] = at
                self.open_gap["duration_us"] = at - self.open_gap["start_us"]
                self.gaps.append(self.open_gap)
                self.open_gap = None
        elif before and not after:
            self.open_gap = {
                "start_us": at,
                "event": event,
                "reason": integer(record, "reason", source),
                "pacer_queue": queue_snapshot(record, source),
            }

    def summary(self) -> dict:
        issues = set(self.issues)
        if self.last_sequence is None:
            issues.add("no_readiness_events")
        if self.first_ready_us is None:
            issues.add("first_ready_not_observed")
        if self.terminal_ms is None:
            issues.add("session_end_not_observed")
        elif (
            self.last_readiness_us is not None
            and self.terminal_ms * 1000 + 999 < self.last_readiness_us
        ):
            issues.add("terminal_before_readiness")
        completed = sum(gap["duration_us"] for gap in self.gaps)
        result = {
            "session": self.number,
            "occurrence": self.occurrence,
            "trace_id": self.trace_id,
            "coverage": "complete" if not issues else "partial",
            "issues": sorted(issues),
            "first_ready_us": self.first_ready_us,
            "last_observed_us_lower_bound": self.last_observed_us,
            "terminal_fault": self.terminal_fault,
            "falls": len(self.gaps) + int(self.open_gap is not None),
            "display_expired_falls": sum(gap["reason"] == DISPLAY_EXPIRED for gap in self.gaps)
            + int(self.open_gap is not None and self.open_gap["reason"] == DISPLAY_EXPIRED),
            "completed_unready_us": completed,
            "max_completed_gap_us": max((gap["duration_us"] for gap in self.gaps), default=0),
            "completed_gaps": self.gaps,
            "terminal_gap_us_range": None,
            "open_gap_observed_lower_bound_us": None,
            "total_unready_us_range": [completed, completed] if not issues else None,
        }
        if self.open_gap is not None:
            start = self.open_gap["start_us"]
            if self.terminal_ms is not None:
                lower = max(0, self.terminal_ms * 1000 - start)
                upper = max(0, self.terminal_ms * 1000 + 999 - start)
                result["terminal_gap_us_range"] = [lower, upper]
                if not issues:
                    result["total_unready_us_range"] = [completed + lower, completed + upper]
            else:
                result["open_gap_observed_lower_bound_us"] = max(
                    0, self.last_observed_us - start
                )
                result["total_unready_us_range"] = None
            result["open_gap"] = self.open_gap
        return result


def analyze(path: Path) -> dict:
    sessions: list[Session] = []
    current: dict[int, Session] = {}
    by_trace_id: dict[int, Session] = {}
    occurrences: dict[int, int] = {}
    log_loss = False
    with path.open(encoding="utf-8") as stream:
        for line_number, line in enumerate(stream, 1):
            if not line.strip():
                continue
            source = f"{path}:{line_number}"
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                raise ValueError(f"{source}: invalid JSON: {error.msg}") from error
            if not isinstance(record, dict):
                raise ValueError(f"{source}: expected a JSON object")
            if record.get("log_dropped", 0) or record.get("log_failed", False):
                log_loss = True
            if "readiness_event" not in record and "bitrate_kbps" not in record:
                continue
            number = integer(record, "session", source)
            trace_id = record.get("trace_id")
            if trace_id is not None:
                trace_id = integer(record, "trace_id", source)
                session = by_trace_id.get(trace_id)
                if session is not None and session.number != number:
                    raise ValueError(f"{source}: trace_id changed session number")
            else:
                session = current.get(number)
                if session is not None:
                    restart = session.trace_id is not None or session.terminal_ms is not None
                    if "readiness_event" in record:
                        at = integer(record, "elapsed_us", source)
                        sequence = integer(record, "sequence", source)
                        restart |= sequence == 1 and session.last_sequence is not None
                        restart |= session.last_sequence is not None and (
                            sequence <= session.last_sequence
                            or at < (session.last_readiness_us or 0)
                        )
                    else:
                        elapsed_ms = integer(record, "elapsed_ms", source)
                        restart |= elapsed_ms * 1000 + 999 < session.last_observed_us
                    if restart:
                        session = None
            if session is None:
                occurrences[number] = occurrences.get(number, 0) + 1
                session = Session(number, occurrences[number], trace_id)
                if trace_id is None and occurrences[number] > 1 and current[number].terminal_ms is None:
                    session.issues.add("legacy_run_boundary_inferred")
                sessions.append(session)
                current[number] = session
                if trace_id is not None:
                    by_trace_id[trace_id] = session
            if "readiness_event" in record:
                session.observe_readiness(record, source)
            else:
                session.observe_peer(record, source)
    if log_loss:
        for session in sessions:
            session.issues.add("diagnostics_loss_in_file")
    return {"file": str(path), "sessions": [session.summary() for session in sessions]}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", metavar="LOG.jsonl", type=Path, nargs="+")
    args = parser.parse_args()
    try:
        for path in args.logs:
            print(json.dumps(analyze(path), separators=(",", ":")))
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

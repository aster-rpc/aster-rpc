#!/usr/bin/env python3
"""Persistent six-language RPC qualification, with an explicit preflight mode.

Run only after preparing the interop programs. This harness owns its processes
and collector; it never changes host configuration or builds native artifacts.
Queue128 refers exclusively to the measured adapter admission queue.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import gzip
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import queue
import re
import signal
import subprocess
import sys
import threading
import time


LANGUAGES = ("rust", "python", "ts", "go", "java", "dotnet")


def digest(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for part in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(part)
    return h.hexdigest()


class Evidence:
    def __init__(self, root: Path):
        self.root = root
        root.mkdir(parents=True, exist_ok=False)
        self.log = (root / "events.jsonl").open("w")
        self.lock = threading.Lock()

    def write(self, kind: str, **fields):
        entered = time.monotonic()
        with self.lock:
            acquired = time.monotonic()
            self.log.write(json.dumps(dict(kind=kind, monotonic=acquired, **fields)) + "\n")
            flush_started = time.monotonic()
            self.log.flush()
            flushed = time.monotonic()
        return dict(evidence_entered_monotonic=entered,
                    evidence_lock_acquired_monotonic=acquired,
                    evidence_flush_started_monotonic=flush_started,
                    evidence_flush_completed_monotonic=flushed)


class Peer:
    def __init__(self, name, cmd, cwd, env, evidence):
        self.name = name
        self.evidence = evidence
        self.lines = queue.Queue(maxsize=32)
        self.stderr = (evidence.root / f"{name}.stderr.log").open("w")
        self.stdout = (evidence.root / f"{name}.stdout.log").open("w")
        self.process = subprocess.Popen(cmd, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.stderr, text=True, bufsize=1, start_new_session=True)
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()

    def read(self):
        for line in self.process.stdout:
            self.stdout.write(line)
            self.stdout.flush()
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                value = line.strip() if line.startswith("ADDRESS ") else None
            if value is not None:
                self.lines.put(value)
        self.lines.put(dict(phase="process_exit", returncode=self.process.wait()))

    def receive(self, timeout=20):
        try:
            value = self.lines.get(timeout=timeout)
        except queue.Empty as exc:
            raise TimeoutError(f"{self.name}: no protocol reply within {timeout}s") from exc
        if isinstance(value, dict) and value.get("phase") in ("campaign_error", "process_exit"):
            raise AssertionError(f"{self.name}: {value}")
        return value

    def send(self, command):
        self.process.stdin.write(json.dumps(command) + "\n")
        self.process.stdin.flush()

    def ask(self, command, timeout=20):
        self.send(command)
        return self.receive(timeout)

    def stop(self, force=False):
        if self.process.poll() is None:
            if force:
                os.killpg(self.process.pid, signal.SIGKILL)
            else:
                self.process.stdin.close()
                try:
                    self.process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=5)
        if not self.process.stdin.closed:
            self.process.stdin.close()
        self.reader.join(timeout=5)
        assert not self.reader.is_alive(), self.name
        self.process.stdout.close()
        self.stdout.close()
        self.stderr.close()


class Collector:
    def __init__(self, evidence):
        self.evidence = evidence
        self.lock = threading.Lock()
        self.mode = "healthy"
        self.markers = {}
        self.release = threading.Event()
        self.release.set()
        self.port = 0
        self.server = None
        self.removed = None
        self.handlers = threading.Condition()
        self.pending_handlers = 0
        self.handler_errors = []
        self.start()

    def start(self):
        collector = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                handler_started = time.monotonic()
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                body_read_completed = time.monotonic()
                if self.headers.get("Content-Encoding") == "gzip":
                    body = gzip.decompress(body)
                with collector.lock:
                    mode = collector.mode
                if mode == "stall":
                    collector.release.wait(timeout=15)
                    return
                status = 503 if mode == "http503" else 200
                accepted = None
                if status == 200 and self.path.endswith("/v1/logs"):
                    found = {x.decode() for x in re.findall(rb"campaign:(?:rust|python|ts|go|java|dotnet):[0-9]+;", body)}
                    now = time.monotonic()
                    with collector.lock:
                        for marker in found:
                            collector.markers.setdefault(marker, now)
                        while len(collector.markers) > 2048:
                            del collector.markers[next(iter(collector.markers))]
                    accepted = now
                response_error = None
                try:
                    self.send_response(status)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                except (BrokenPipeError, ConnectionResetError):
                    response_error = "peer_closed"
                response_completed = time.monotonic()
                if accepted is not None:
                    # The collector accepts into memory before acknowledging;
                    # evidence-file latency must not become exporter latency.
                    # close() joins these handlers and rejects missing evidence.
                    timing = collector.evidence.write(
                        "collector_accept", markers=sorted(found), bytes=len(body),
                        handler_started_monotonic=handler_started,
                        body_read_completed_monotonic=body_read_completed,
                        accepted_monotonic=accepted,
                        response_completed_monotonic=response_completed)
                    collector.evidence.write(
                        "collector_response", markers=sorted(found), status=status,
                        handler_started_monotonic=handler_started,
                        body_read_completed_monotonic=body_read_completed,
                        response_completed_monotonic=response_completed,
                        response_error=response_error, **timing)

        class Server(http.server.ThreadingHTTPServer):
            allow_reuse_address = True
            daemon_threads = True

            def process_request(self, request, client_address):
                with collector.handlers:
                    collector.pending_handlers += 1
                    collector.handlers.notify_all()
                try:
                    super().process_request(request, client_address)
                except Exception:
                    with collector.handlers:
                        collector.pending_handlers -= 1
                        collector.handlers.notify_all()
                    raise

            def process_request_thread(self, request, client_address):
                try:
                    super().process_request_thread(request, client_address)
                finally:
                    with collector.handlers:
                        collector.pending_handlers -= 1
                        collector.handlers.notify_all()

            def handle_error(self, request, client_address):
                with collector.handlers:
                    collector.handler_errors.append(sys.exc_info()[1])

        self.server = Server(("127.0.0.1", self.port), Handler)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    @property
    def endpoint(self):
        return f"http://127.0.0.1:{self.port}"

    def fault(self, mode):
        assert self.mode == "healthy"
        with self.lock:
            self.mode = mode
            self.removed = None
            self.release.clear()
        if mode == "refusal":
            self.server.shutdown()
            self.server.server_close()
            self.server = None
        self.evidence.write("collector_fault", mode=mode)

    def restore(self):
        self.release.set()
        if self.server is None:
            self.start()
        with self.lock:
            self.mode = "healthy"
            self.removed = time.monotonic()
        self.evidence.write("collector_restored", endpoint=self.endpoint)

    def close(self):
        self.release.set()
        if self.server is not None:
            self.server.shutdown()
            self.server.server_close()
        with self.handlers:
            if not self.handlers.wait_for(lambda: self.pending_handlers == 0, timeout=20):
                raise TimeoutError("collector handlers did not drain their evidence within 20s")
            if self.handler_errors:
                raise RuntimeError("collector handler failed; evidence is incomplete") from self.handler_errors[0]


def proc_fields(pid):
    # Linux comm can contain spaces and parentheses; split after its final ')'.
    return (Path("/proc") / str(pid) / "stat").read_text().rsplit(") ", 1)[1].split()


def owned_pid(pid, launcher):
    assert isinstance(pid, int) and not isinstance(pid, bool) and pid > 0, pid
    current = pid
    for _ in range(32):
        if current == launcher:
            return True
        current = int(proc_fields(current)[1])
        if current <= 1:
            break
    return False


def resources(pid):
    root = Path("/proc") / str(pid)
    stat = proc_fields(pid)
    return dict(pid=pid, rss=int(stat[21]) * os.sysconf("SC_PAGE_SIZE"),
                threads=int(stat[17]), fds=len(list((root / "fd").iterdir())))


def verify_native(lang, pid, entry, evidence, *, rehash=True):
    origin = Path(entry["path"])
    assert origin.is_absolute() and str(origin.resolve(strict=True)) == str(origin), entry
    if lang == "rust":
        loaded = str((Path("/proc") / str(pid) / "exe").resolve(strict=True))
        assert loaded == str(origin), (lang, loaded, entry)
    else:
        mapped = set()
        for line in (Path("/proc") / str(pid) / "maps").read_text().splitlines():
            fields = line.split(maxsplit=5)
            if len(fields) == 6 and fields[5].startswith("/"):
                mapped.add(fields[5])
        assert str(origin) in mapped, (lang, pid, entry, sorted(mapped))
        assert str(origin) + " (deleted)" not in mapped, (lang, pid, entry)
        competing_origins = {value for value in mapped if Path(value.removesuffix(" (deleted)")).name == origin.name}
        assert competing_origins == {str(origin)}, (lang, "ambiguous loaded native origin", competing_origins)
    actual_hash = digest(origin) if rehash else entry["sha256"]
    assert actual_hash == entry["sha256"], (lang, actual_hash, entry)
    evidence.write("loaded_native", language=lang, pid=pid, origin=str(origin),
                   sha256=actual_hash, verification="proc_exe" if lang == "rust" else "proc_maps",
                   hash_rechecked=rehash)


def p99(hist):
    total = sum(hist)
    assert total
    floor = (total * 99 + 99) // 100
    accumulated = 0
    for exponent, count in enumerate(hist):
        accumulated += count
        if accumulated >= floor:
            return 2 ** exponent
    raise AssertionError("invalid histogram")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--manifest", type=Path, required=True, help="frozen pre-run artifact/profile manifest")
    ap.add_argument("--preflight", action="store_true")
    ap.add_argument("--seconds", type=float, default=3600)
    ap.add_argument("--warmup", type=float, default=120)
    ap.add_argument("--languages", default=",".join(LANGUAGES))
    args = ap.parse_args()
    langs = tuple(args.languages.split(","))
    if not langs or len(set(langs)) != len(langs) or any(lang not in LANGUAGES for lang in langs):
        ap.error("languages must be a unique nonempty selection of the six supported adapters")
    if args.seconds <= 0 or args.warmup <= 0:
        ap.error("seconds and warmup must be positive")
    if not args.preflight and (langs != LANGUAGES or args.seconds < 3600 or args.warmup < 120):
        ap.error("qualification requires all six languages, >=120s baseline and >=3600s campaign")
    if not Path("/proc/self/maps").is_file():
        ap.error("campaign qualification requires Linux /proc origin and resource evidence")
    root = Path(__file__).resolve().parents[2]
    manifest = json.loads(args.manifest.read_text())
    assert manifest.get("queue_scope") == "adapter_admission", "manifest must identify adapter admission queue scope"
    loaded_native = manifest["loaded_native"]
    for lang in set(langs) | {"rust"}:
        entry = loaded_native[lang]
        origin = Path(entry["path"])
        assert origin.is_absolute() and str(origin.resolve(strict=True)) == entry["path"], entry
        assert digest(origin) == entry["sha256"], (lang, entry)
        assert any(artifact["path"] == entry["path"] and artifact["sha256"] == entry["sha256"]
                   for artifact in manifest["artifacts"]), (lang, "native origin missing from frozen artifacts")
    for entry in manifest["artifacts"]:
        assert digest(Path(entry["path"])) == entry["sha256"], entry
    evidence = Evidence(args.output.resolve())
    evidence.write("manifest", profile=manifest, exploratory=args.preflight)
    spec = importlib.util.spec_from_file_location("interop", root / "tests/interop/run.py")
    driver = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(driver)
    driver._mvn_offline = False
    collector = Collector(evidence)
    env = dict(driver.ENV, ASTER_CAMPAIGN_COLLECTOR=collector.endpoint, ASTER_LOG="warn", ASTER_LOG_STDERR="1")
    servers, clients, identities, pids, generations, reference = {}, {}, {}, {}, {}, {}
    baseline = {lang: dict(completed=0, seconds=0.0, hist=[0] * 27) for lang in langs}
    measured = {lang: dict(completed=0, seconds=0.0, hist=[0] * 27) for lang in langs}
    window_id = 0
    timers = []
    fault_modes_completed = set()
    restart_languages_completed = set()
    slow_languages = set()
    slow_occurrences_by_language = {lang: set() for lang in langs}
    final_healthy_window = False
    pending_restart = None
    pool = None
    result = None

    def server(lang):
        generation = generations.get(lang, 0)
        cmd, cwd = driver.command("rust", "server")
        peer = Peer(f"server-{lang}-{generation}", cmd, cwd, env, evidence)
        servers[lang] = peer
        ticket = peer.receive()
        assert isinstance(ticket, str) and ticket.startswith("ADDRESS "), ticket
        servers[lang] = peer
        verify_native("rust", peer.process.pid, loaded_native["rust"], evidence)
        if generation > 0:
            reference[("server", lang, peer.process.pid)] = resources(peer.process.pid)
        return ticket[8:]

    def idle(check_budgets):
        for lang in langs:
            reply = clients[lang].ask(dict(cmd="pause", phase="healthy", expected_flush_errors=[]))
            assert reply.get("owner_identity") == identities[lang] and reply.get("pid") == pids[lang], (lang, reply)
            assert reply.get("active_calls") == 0 and reply.get("unexpected_errors") == 0, (lang, reply)
        time.sleep(2)
        for lang in langs:
            status = servers[lang].ask(dict(cmd="campaign_status"))
            assert status.get("phase") == "campaign_server_status" and status.get("pid") == servers[lang].process.pid, (lang, status)
            assert status.get("active_calls_scope") == "native_echo_handlers" and status.get("active_holds_scope") == "native_hold_handlers", status
            assert status.get("active_calls") == status.get("active_holds") == 0, (lang, status)
            for kind, pid in (("client", pids[lang]), ("server", servers[lang].process.pid)):
                point = resources(pid)
                key = (kind, lang, pid)
                if key not in reference:
                    reference[key] = point
                start = reference[key]
                if check_budgets:
                    assert point["rss"] - start["rss"] <= max(64 * 1024 ** 2, start["rss"] * .1), (key, start, point)
                    assert point["fds"] - start["fds"] <= 8, (key, start, point)
                    assert point["threads"] - start["threads"] <= 4, (key, start, point)
                evidence.write("idle_resources", process=kind, language=lang, baseline=start, observed=point)

    try:
        for lang in langs:
            generations[lang] = 0
            ticket = server(lang)
            cmd, cwd = driver.command(lang, "campaign-client", ticket)
            peer = Peer(f"client-{lang}", cmd, cwd, env, evidence)
            clients[lang] = peer
            ready = peer.receive(timeout=30)
            assert ready.get("phase") == "campaign_ready" and isinstance(ready.get("owner_identity"), str) and ready["owner_identity"], (lang, ready)
            assert ready.get("generation") == 0 and owned_pid(ready.get("pid"), peer.process.pid), (lang, ready)
            verify_native(lang, ready["pid"], loaded_native[lang], evidence)
            clients[lang] = peer
            identities[lang], pids[lang] = ready["owner_identity"], ready["pid"]
            evidence.write("client_ready", adapter=lang, **ready)
        start = time.monotonic()
        qualifying_start = None
        next_fault = 15 if args.preflight else 300
        slow_interval = 30 if args.preflight else 600
        next_slow = 15 if args.preflight else 300
        expected_slow_occurrences = 0
        due = next_slow
        while due < args.seconds:
            expected_slow_occurrences += 1
            due += slow_interval
        next_idle = 60
        fault_index = 0
        slow_until = 0
        slow_occurrence = 0
        active_slow_occurrence = 0
        pending_restart = None
        recovery = None
        while (qualifying_start is None or time.monotonic() - qualifying_start < args.seconds
               or recovery is not None or not final_healthy_window):
            elapsed = time.monotonic() - start
            if qualifying_start is None and elapsed >= args.warmup:
                idle(False)
                qualifying_start = time.monotonic()
                next_idle = 60
                evidence.write("qualification_started")
            qualifying_elapsed = 0 if qualifying_start is None else time.monotonic() - qualifying_start
            restart_lang = None
            campaign_running = qualifying_start is not None and qualifying_elapsed < args.seconds
            if (campaign_running and qualifying_elapsed >= next_fault and collector.mode == "healthy"
                    and recovery is None):
                mode = ("http503", "refusal", "stall")[fault_index % 3]
                restart_lang = langs[fault_index % len(langs)]
                fault_index += 1
                next_fault += 15 if args.preflight else 300
                collector.fault(mode)
                timer = threading.Timer(10, collector.restore)
                timer.start()
                timers.append(timer)
                recovery = dict(collector=True, mode=mode)
                final_healthy_window = False
            # Fault and slow schedules are independent due-state machines. If
            # their deadlines coincide, run the compounded profile instead of
            # letting collector recovery starve the slow-consumer gate.
            if campaign_running and qualifying_elapsed >= next_slow:
                next_slow += slow_interval
                slow_occurrence += 1
                active_slow_occurrence = slow_occurrence
                slow_until = max(slow_until, time.monotonic() + 30)
                evidence.write("slow_profile_started", occurrence=slow_occurrence, until=slow_until,
                               overlaps_recovery=recovery is not None,
                               next_due=next_slow)
            if qualifying_start is not None and not campaign_running:
                slow_until = 0
            window_id += 1
            duration = 5.0
            began = time.monotonic()
            exporter_failing = collector.mode != "healthy"
            slow_window = began < slow_until
            phase = "fault" if exporter_failing or slow_window or restart_lang else "healthy"
            if qualifying_start is not None and phase != "healthy":
                final_healthy_window = False
            commands = {}
            for lang in langs:
                command = dict(cmd="baseline" if qualifying_start is None else "run", duration_s=duration,
                    phase=phase, window_id=str(window_id), slow_ms=50 if began < slow_until else 0,
                    expected_errors=(["unavailable", "deadline_exceeded", "cancelled"]
                        + (["planned_stream_cancel"] if lang == "rust" else [])
                        + (["context_deadline_exceeded"] if lang == "go" else [])) if lang == restart_lang else [],
                    expected_flush_errors=["timeout", "sdk"] if exporter_failing else [])
                command["generation"] = generations[lang]
                commands[lang] = command
                clients[lang].send(command)
            if restart_lang:
                def restart(lang=restart_lang):
                    time.sleep(1)
                    servers[lang].stop(force=True)
                    generations[lang] += 1
                    ticket = server(lang)
                    return lang, ticket, time.monotonic()
                pool = concurrent.futures.ThreadPoolExecutor(max_workers=1)
                pending_restart = pool.submit(restart)
            replies = {}
            for lang in langs:
                row = clients[lang].receive(timeout=duration + 8)
                assert row.get("phase") == "campaign_window", (lang, row)
                assert row.get("owner_identity") == identities[lang] and row.get("pid") == pids[lang] and row.get("generation") == commands[lang]["generation"], (lang, row)
                assert row.get("active_calls") == 0 and row.get("unexpected_errors") == 0 and row.get("integrity_failures") == 0, (lang, row)
                assert row.get("queue_scope") == "adapter_admission" and row.get("queue_depth") == 0, (lang, row)
                assert row.get("queue_capacity") == 128 and 0 <= row["queue_high_water"] <= 128, (lang, row)
                assert row["latency_histogram"].get("unit") == "us" and row["latency_histogram"].get("upper_bounds_powers_of_two") is True, (lang, row)
                histogram = row["latency_histogram"]["counts"]
                assert all(isinstance(value, int) and value >= 0 for value in histogram), (lang, row)
                assert row.get("log_marker") == f"campaign:{lang}:{window_id};", (lang, row)
                assert row.get("app_log_emitted") == row.get("completed"), (lang, row)
                assert "first_success_elapsed_ms" in row and "last_success_elapsed_ms" in row, (lang, row)
                if row["unary"] + row["stream"]:
                    first, last = row["first_success_elapsed_ms"], row["last_success_elapsed_ms"]
                    assert isinstance(first, (int, float)) and isinstance(last, (int, float)) and 0 <= first <= last <= (duration + 4) * 1000, (lang, row)
                classes = row.get("error_classes")
                assert isinstance(classes, dict), (lang, row)
                allowed = set(commands[lang]["expected_errors"])
                assert allowed <= {"unavailable", "deadline_exceeded", "cancelled", "planned_stream_cancel", "context_deadline_exceeded"}, allowed
                assert not any(classes.get(local, 0) for local in ("local_timeout", "local_error", "other")), (lang, row)
                assert all(name in allowed or (name == "cancelled" and count <= row["cancel"])
                           for name, count in classes.items() if count), (lang, row)
                planned = classes.get("planned_stream_cancel", 0)
                assert planned == row.get("planned_stream_cancellations", 0), (lang, row)
                if planned:
                    assert lang == "rust" and lang == restart_lang and row["active_calls"] == 0 and row["queue_depth"] == 0, (lang, row)
                context_deadlines = classes.get("context_deadline_exceeded", 0)
                if context_deadlines:
                    assert lang == "go" and lang == restart_lang and row["active_calls"] == 0 and row["queue_depth"] == 0, (lang, row)
                if slow_window:
                    assert row["stream"] > 0, (lang, "slow profile produced no complete stream")
                    slow_languages.add(lang)
                    slow_occurrences_by_language[lang].add(active_slow_occurrence)
                assert len(histogram) == 27 and sum(histogram) == row["completed"] == row["unary"] + row["stream"] + row["cancel"], (lang, row)
                assert row["attempted"] >= row["completed"] and row["completed"] > 0, (lang, row)
                evidence.write("window", language=lang, window_id=window_id, warmup=qualifying_start is None, result=row)
                replies[lang] = row
                if phase == "healthy":
                    assert row.get("app_log_emitted", 0) > 0, (lang, row)
                    assert row["log_marker"] in collector.markers, (lang, row)
                    aggregate = baseline[lang] if qualifying_start is None else measured[lang]
                    aggregate["completed"] += row["completed"]
                    aggregate["seconds"] += row.get("duration_s", duration)
                    aggregate["hist"] = [a + b for a, b in zip(aggregate["hist"], histogram)]
            if phase == "healthy" and qualifying_start is not None:
                final_healthy_window = True
            if pending_restart:
                lang, ticket, removed = pending_restart.result(timeout=15)
                pool.shutdown()
                reply = clients[lang].ask(dict(cmd="replace_peer", ticket=ticket, generation=generations[lang]), timeout=8)
                assert reply.get("phase") in ("campaign_peer", "campaign_peer_replaced"), (lang, reply)
                assert reply.get("owner_identity") == identities[lang] and reply.get("pid") == pids[lang] and reply.get("generation") == generations[lang], (lang, reply)
                recovery["peer"] = dict(language=lang, removed=removed)
                pending_restart = None
            elif recovery:
                if recovery.get("collector") and not exporter_failing and collector.removed is not None and began >= collector.removed:
                    assert collector.removed is not None
                    for lang, row in replies.items():
                        delivery = collector.markers[row["log_marker"]] - collector.removed
                        assert 0 <= delivery <= 10, (lang, delivery)
                        evidence.write("collector_recovery", language=lang, seconds=delivery)
                    recovery["collector"] = False
                if recovery.get("peer"):
                    lang = recovery["peer"]["language"]
                    assert replies[lang]["unary"] + replies[lang]["stream"] > 0, (lang, "peer recovery has no actual RPC reply")
                    delay = began + replies[lang]["first_success_elapsed_ms"] / 1000 - recovery["peer"]["removed"]
                    assert 0 <= delay <= 10, (lang, delay)
                    evidence.write("peer_recovery", language=lang, seconds=delay)
                    restart_languages_completed.add(lang)
                    del recovery["peer"]
                if not recovery.get("collector") and not recovery.get("peer"):
                    fault_modes_completed.add(recovery["mode"])
                    recovery = None
            if qualifying_start is not None and qualifying_elapsed >= next_idle and phase == "healthy" and recovery is None:
                idle(True)
                next_idle += 60
        for timer in timers:
            timer.join(timeout=15)
        if collector.mode != "healthy":
            collector.restore()
        assert recovery is None and pending_restart is None and final_healthy_window, "final recovery incomplete"
        expected_slow_ids = set(range(1, expected_slow_occurrences + 1))
        assert slow_occurrence == expected_slow_occurrences, (slow_occurrence, expected_slow_occurrences)
        assert all(seen == expected_slow_ids for seen in slow_occurrences_by_language.values()), slow_occurrences_by_language
        if not args.preflight:
            assert fault_modes_completed == {"http503", "refusal", "stall"}, fault_modes_completed
            assert restart_languages_completed == set(LANGUAGES), restart_languages_completed
            assert slow_languages == set(LANGUAGES), slow_languages
        idle(True)
        for lang in langs:
            verify_native(lang, pids[lang], loaded_native[lang], evidence)
            verify_native("rust", servers[lang].process.pid, loaded_native["rust"], evidence)
        for lang in langs:
            b, m = baseline[lang], measured[lang]
            assert b["completed"] and m["completed"], (lang, b, m)
            ratio = (m["completed"] / m["seconds"]) / (b["completed"] / b["seconds"])
            latency_ratio = p99(m["hist"]) / p99(b["hist"])
            assert ratio >= .8 and latency_ratio <= 2, (lang, ratio, latency_ratio)
            evidence.write("healthy_budget", language=lang, throughput_fraction=ratio, p99_multiple=latency_ratio)
        for lang in langs:
            row = clients[lang].ask(dict(cmd="stop", phase="healthy", expected_flush_errors=[]))
            assert row.get("phase") in ("campaign_stopped", "campaign_stop") and row.get("active_calls") == 0 and row.get("unexpected_errors") == 0, (lang, row)
            assert row.get("owner_identity") == identities[lang] and row.get("pid") == pids[lang], (lang, row)
            # The stop acknowledgement ends the protocol. Close our owned
            # writer so stdin-backed readline implementations can release
            # their final event-loop handle, then retain the bounded exit gate.
            clients[lang].process.stdin.close()
            clients[lang].process.wait(timeout=8)
            assert clients[lang].process.returncode == 0, lang
        result = dict(status="PASS", qualification="EXPLORATORY" if args.preflight else "PROFILE_PASS",
                      seconds=time.monotonic() - qualifying_start, languages=langs,
                      queue_scope="adapter_admission", native_queue128_claim=False, fault_cycles=fault_index,
                      completed_fault_modes=sorted(fault_modes_completed), recovered_peer_languages=sorted(restart_languages_completed),
                      slow_profile_languages=sorted(slow_languages), slow_profile_occurrences=slow_occurrence,
                      expected_slow_profile_occurrences=expected_slow_occurrences,
                      slow_profile_occurrences_by_language={lang: sorted(seen) for lang, seen in slow_occurrences_by_language.items()},
                      final_recovery_complete=True)
    finally:
        for timer in timers:
            timer.cancel()
        # A restart task can replace a server dictionary entry; join it before
        # enumerating owned processes so an assertion failure cannot orphan it.
        if pool is not None:
            pool.shutdown(wait=True, cancel_futures=True)
        cleanup_errors = []
        for peer in list(clients.values()) + list(servers.values()):
            try:
                peer.stop()
            except Exception as exc:
                cleanup_errors.append(f"{peer.name}: {exc}")
        try:
            collector.close()
        finally:
            # A drained write failure rejects PASS but can close its stream.
            # A timed-out handler may still be writing; do not race that I/O.
            with collector.handlers:
                if collector.pending_handlers == 0:
                    evidence.log.close()
        if cleanup_errors:
            raise RuntimeError("owned process cleanup failed: " + "; ".join(cleanup_errors))
    # A PASS artifact is a terminal claim: all pending recovery and owned
    # process cleanup must have succeeded before it is published.
    assert result is not None
    (evidence.root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()

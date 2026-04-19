package site.aster.examples.missioncontrol.guide

import java.util.concurrent.TimeUnit
import kotlin.system.exitProcess
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.future.await
import kotlinx.coroutines.runBlocking
import site.aster.client.AsterClient
import site.aster.client.ClientSession
import site.aster.codec.ForyCodec
import site.aster.examples.missioncontrol.AgentSessionDispatcher
import site.aster.examples.missioncontrol.MissionControlDispatcher
import site.aster.examples.missioncontrol.Server
import site.aster.examples.missioncontrol.types.Assignment
import site.aster.examples.missioncontrol.types.Heartbeat
import site.aster.examples.missioncontrol.types.IngestResult
import site.aster.examples.missioncontrol.types.LogEntry
import site.aster.examples.missioncontrol.types.MetricPoint
import site.aster.examples.missioncontrol.types.StatusRequest
import site.aster.examples.missioncontrol.types.StatusResponse
import site.aster.examples.missioncontrol.types.SubmitLogResult
import site.aster.examples.missioncontrol.types.TailRequest
import site.aster.node.NodeAddr

/**
 * Mission Control guide integration test — Kotlin client.
 *
 * Drives a running Mission Control server (Python, TypeScript, or Java) through every chapter of
 * the guide and reports a pass/fail tally. Invoked by
 * `tests/integration/mission_control/run_matrix.sh` whenever the Kotlin client is in the matrix.
 *
 * Exit codes: `0` all tests passed, `1` at least one test failed, `2` setup/usage error.
 *
 * Currently implements Ch1–Ch4 (dev mode). Ch5 (auth) and Ch6 (gen-client) land in later phases.
 */

private var quiet = false
private var passCount = 0
private var failCount = 0

private fun ok(label: String) {
  passCount++
  if (!quiet) println("  \u001B[32m✓\u001B[0m $label")
}

private fun fail(label: String, msg: String) {
  failCount++
  println("  \u001B[31m✗\u001B[0m $label: $msg")
}

private fun section(name: String) {
  if (!quiet) println("\n\u001B[1m$name\u001B[0m")
}

private fun unwrap(t: Throwable): Throwable =
    if (t is java.util.concurrent.ExecutionException && t.cause != null) t.cause!! else t

// ─── Chapter 1: unary ────────────────────────────────────────────────────────

private suspend fun testCh1Unary(client: AsterClient, addr: NodeAddr) {
  try {
    val r: StatusResponse =
        client
            .call<StatusRequest, StatusResponse>(
                addr,
                MissionControlDispatcher.SERVICE_NAME,
                "getStatus",
                StatusRequest("edge-7"),
                StatusResponse::class.java)
            .orTimeout(15, TimeUnit.SECONDS)
            .await()
    when {
      r.agentId() != "edge-7" -> fail("Ch1 getStatus", "agent_id mismatch: $r")
      r.status() != "running" -> fail("Ch1 getStatus", "status mismatch: $r")
      r.uptimeSecs() < 0L -> fail("Ch1 getStatus", "uptime_secs invalid: $r")
      else -> ok("Ch1 getStatus returns typed response")
    }
  } catch (e: Throwable) {
    fail("Ch1 getStatus", unwrap(e).toString())
  }
}

// ─── Chapter 2: streaming (submitLog + tailLogs) ────────────────────────────

/**
 * Ch2b: open a live tailLogs stream, submit a marker entry while the stream is open, verify the
 * first delivered entry is exactly the marker we just submitted. This is the python-parity variant
 * that catches "stream yields stale buffered entries". The server's `tailLogs` exits after ~250ms
 * of queue idle, so we submit within that window and wait for the buffered result list.
 *
 * Run BEFORE Ch2a so Ch2a's submitted log doesn't pollute this stream.
 */
private suspend fun testCh2bTailLogs(client: AsterClient, addr: NodeAddr) {
  val expectedMsg = "kt-ch2-tail-${System.nanoTime()}"
  val now = System.currentTimeMillis() / 1000.0

  val streamFuture =
      client
          .callServerStream<TailRequest, LogEntry>(
              addr,
              MissionControlDispatcher.SERVICE_NAME,
              "tailLogs",
              TailRequest("", "info"),
              LogEntry::class.java)
          .orTimeout(15, TimeUnit.SECONDS)

  // Let the stream open before submitting. If we submit first the server may drain the queue and
  // idle out before we've opened the stream, which would race the test.
  delay(150L)

  try {
    client
        .call<LogEntry, SubmitLogResult>(
            addr,
            MissionControlDispatcher.SERVICE_NAME,
            "submitLog",
            LogEntry(now, "info", expectedMsg, "edge-7"),
            SubmitLogResult::class.java)
        .orTimeout(5, TimeUnit.SECONDS)
        .await()
  } catch (e: Throwable) {
    streamFuture.cancel(true)
    fail("Ch2b tailLogs setup", "submitLog failed: ${unwrap(e)}")
    return
  }

  val entries: List<LogEntry> =
      try {
        streamFuture.await()
      } catch (e: Throwable) {
        fail("Ch2b tailLogs", unwrap(e).toString())
        return
      }

  if (entries.isEmpty()) {
    fail("Ch2b tailLogs", "no entries received within the idle window")
    return
  }
  val first = entries[0]
  if (first.message() != expectedMsg) {
    fail(
        "Ch2b tailLogs",
        "first entry was '${first.message()}', expected '$expectedMsg' — stream is yielding stale/buffered entries")
    return
  }
  ok("Ch2b tailLogs received live entry ($expectedMsg)")
}

private suspend fun testCh2aSubmitLog(client: AsterClient, addr: NodeAddr) {
  try {
    val r: SubmitLogResult =
        client
            .call<LogEntry, SubmitLogResult>(
                addr,
                MissionControlDispatcher.SERVICE_NAME,
                "submitLog",
                LogEntry(
                    System.currentTimeMillis() / 1000.0, "info", "ch2 standalone submit", "edge-7"),
                SubmitLogResult::class.java)
            .orTimeout(15, TimeUnit.SECONDS)
            .await()
    if (!r.accepted()) {
      fail("Ch2a submitLog", "unexpected response: $r")
      return
    }
    ok("Ch2a submitLog accepted")
  } catch (e: Throwable) {
    fail("Ch2a submitLog", unwrap(e).toString())
  }
}

// ─── Chapter 3: client-streaming ingestMetrics ──────────────────────────────

private suspend fun testCh3ClientStream(client: AsterClient, addr: NodeAddr) {
  val n = 1000
  val now = System.currentTimeMillis() / 1000.0
  val points: List<MetricPoint> =
      (0 until n).map { i -> MetricPoint("cpu.usage", i.toDouble(), now, emptyMap()) }

  try {
    val r: IngestResult =
        client
            .callClientStream<MetricPoint, IngestResult>(
                addr,
                MissionControlDispatcher.SERVICE_NAME,
                "ingestMetrics",
                points,
                IngestResult::class.java)
            .orTimeout(15, TimeUnit.SECONDS)
            .await()
    if (r.accepted() != n) {
      fail("Ch3 ingestMetrics", "expected accepted=$n, got ${r.accepted()} (full: $r)")
      return
    }
    ok("Ch3 ingestMetrics accepted $n")
  } catch (e: Throwable) {
    fail("Ch3 ingestMetrics", unwrap(e).toString())
  }
}

// ─── Chapter 4: session-scoped AgentSession.register ─────────────────────────

private suspend fun testCh4Session(client: AsterClient, addr: NodeAddr) {
  // 4a: register with GPU → train-42
  try {
    val session: ClientSession = client.openSession(addr).orTimeout(15, TimeUnit.SECONDS).await()
    session.use { s ->
      val r: Assignment =
          s.call<Heartbeat, Assignment>(
                  AgentSessionDispatcher.SERVICE_NAME,
                  "register",
                  Heartbeat("gpu-1", listOf("gpu"), 0.5),
                  Assignment::class.java)
              .orTimeout(15, TimeUnit.SECONDS)
              .await()
      if (r.taskId() != "train-42") {
        fail("Ch4 register (gpu)", "expected task_id='train-42', got ${r.taskId()}")
        return
      }
      ok("Ch4 register (gpu) returns train-42")
    }
  } catch (e: Throwable) {
    fail("Ch4 register (gpu)", unwrap(e).toString())
    return
  }

  // 4b: register without GPU → idle
  try {
    val session: ClientSession = client.openSession(addr).orTimeout(15, TimeUnit.SECONDS).await()
    session.use { s ->
      val r: Assignment =
          s.call<Heartbeat, Assignment>(
                  AgentSessionDispatcher.SERVICE_NAME,
                  "register",
                  Heartbeat("cpu-1", listOf("arm64"), 0.2),
                  Assignment::class.java)
              .orTimeout(15, TimeUnit.SECONDS)
              .await()
      if (r.taskId() != "idle") {
        fail("Ch4 register (no gpu)", "expected task_id='idle', got ${r.taskId()}")
        return
      }
      ok("Ch4 register (no gpu) returns idle")
    }
  } catch (e: Throwable) {
    fail("Ch4 register (no gpu)", unwrap(e).toString())
  }
}

// ─── Dev mode runner ─────────────────────────────────────────────────────────

private suspend fun runDevMode(addrStr: String) {
  section("Dev mode (no auth)")

  val addr: NodeAddr =
      try {
        NodeAddr.fromTicket(addrStr)
      } catch (e: Throwable) {
        fail("connect", "failed to parse address ticket: ${unwrap(e)}")
        return
      }

  val codec = ForyCodec()
  Server.registerWireTypes(codec)
  val client: AsterClient =
      try {
        AsterClient.builder().codec(codec).build().get(15, TimeUnit.SECONDS)
      } catch (e: Throwable) {
        fail("connect", "failed to build client: ${unwrap(e)}")
        return
      }

  try {
    testCh1Unary(client, addr)
    // Ch2b BEFORE Ch2a so Ch2a's submit doesn't pollute Ch2b's tailLogs queue
    testCh2bTailLogs(client, addr)
    testCh2aSubmitLog(client, addr)
    testCh3ClientStream(client, addr)
    testCh4Session(client, addr)
  } finally {
    try {
      client.close()
    } catch (_: Throwable) {
      // best-effort
    }
  }
}

private fun runAuthMode(addrStr: String, keysDir: String) {
  section("Auth mode (Chapter 5)")
  fail("Ch5", "Kotlin auth-mode client not yet implemented (Phase 3b)")
  @Suppress("UNUSED_PARAMETER") addrStr
  @Suppress("UNUSED_PARAMETER") keysDir
}

// ─── Main ────────────────────────────────────────────────────────────────────

fun main(args: Array<String>) {
  val rc = runBlocking(Dispatchers.Default) { mainImpl(args) }
  exitProcess(rc)
}

private suspend fun mainImpl(args: Array<String>): Int {
  if (args.isEmpty()) {
    System.err.println(
        "Usage: test-guide <address> [--mode dev|auth] [--keys-dir DIR] [-q|--quiet]")
    return 2
  }

  val address = args[0]
  var mode = "dev"
  var keysDir = ""
  var i = 1
  while (i < args.size) {
    when (val a = args[i]) {
      "--mode" -> {
        mode = args[++i]
      }
      "--keys-dir" -> {
        keysDir = args[++i]
      }
      "-q",
      "--quiet" -> quiet = true
      else -> {
        System.err.println("Unknown arg: $a")
        return 2
      }
    }
    i++
  }

  if (mode == "auth" && keysDir.isEmpty()) {
    System.err.println("Error: --keys-dir is required for --mode auth")
    return 2
  }

  if (!quiet) {
    val trimmed = if (address.length > 30) address.substring(0, 30) else address
    println("Testing $trimmed... (mode=$mode)")
  }

  when (mode) {
    "dev" -> runDevMode(address)
    "auth" -> runAuthMode(address, keysDir)
    else -> {
      System.err.println("Unknown mode: $mode")
      return 2
    }
  }

  if (!quiet) {
    println(
        "\n\u001B[1mResult:\u001B[0m \u001B[32m$passCount passed\u001B[0m, \u001B[31m$failCount failed\u001B[0m")
  } else {
    println("kt-client $mode: $passCount pass, $failCount fail")
  }
  return if (failCount == 0) 0 else 1
}

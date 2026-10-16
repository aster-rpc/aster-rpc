using System.Diagnostics;
using System.Text.Json;
using Aster.Audit;

namespace Aster.Tests;

public sealed class AuditOutboxTests
{
    private static string PythonExecutable =>
        Environment.GetEnvironmentVariable("ASTER_TEST_PYTHON") is { Length: > 0 } configured
            ? configured
            : OperatingSystem.IsWindows() ? "python" : "python3";

    private sealed class Receiver : IDisposable
    {
        private readonly Process _process;
        public string Endpoint { get; }
        public Receiver(string database, int port = 0)
        {
            var root = new DirectoryInfo(AppContext.BaseDirectory);
            while (root is not null && !File.Exists(Path.Combine(root.FullName, "tests/audit_outbox/receiver.py"))) root = root.Parent;
            Assert.NotNull(root);
            var start = new ProcessStartInfo(PythonExecutable) { RedirectStandardOutput = true, RedirectStandardError = true };
            start.ArgumentList.Add(Path.Combine(root!.FullName, "tests/audit_outbox/receiver.py"));
            start.ArgumentList.Add("--database"); start.ArgumentList.Add(database);
            start.ArgumentList.Add("--port"); start.ArgumentList.Add(port.ToString());
            _process = Process.Start(start)!;
            try { Endpoint = _process.StandardOutput.ReadLineAsync().WaitAsync(TimeSpan.FromSeconds(10)).GetAwaiter().GetResult()!; }
            catch { Dispose(); throw; }
            Assert.StartsWith("http://127.0.0.1:", Endpoint);
        }
        public void Dispose()
        {
            if (!_process.HasExited) _process.Kill(entireProcessTree: true);
            _process.WaitForExit(5000);
            _process.Dispose();
        }
    }

    [Fact(Timeout = 30000)]
    public async Task ReopenDeliversOriginalEventAndReportsTypedError()
    {
        var directory = Directory.CreateTempSubdirectory("aster-audit-dotnet-");
        try
        {
            string database = Path.Combine(directory.FullName, "receiver.sqlite");
            string endpoint;
            using (var receiver = new Receiver(database)) endpoint = receiver.Endpoint;
            using var rt = new Runtime();
            var config = new AuditOutboxConfig(Path.Combine(directory.FullName, "outbox"), new(endpoint));
            string id;
            await using (var box = await AuditOutbox.OpenAsync(rt, config))
            {
                id = await box.SubmitAsync(new(ulong.MaxValue, "dotnet.test", JsonSerializer.SerializeToElement(new { value = 42 })));
                Assert.Equal(1UL, (await box.StatusAsync()).PendingRecords);
                var error = await Assert.ThrowsAsync<AuditOutboxException>(() => box.SubmitAsync(new(0, "large", JsonSerializer.SerializeToElement(new { data = new string('x', 70 * 1024) }))));
                Assert.Equal(AuditErrorKind.EventTooLarge, error.Kind);
            }
            using (var receiver = new Receiver(database, new Uri(endpoint).Port))
            {
                await using var box = await AuditOutbox.OpenAsync(rt, config);
                await box.FlushAsync(TimeSpan.FromSeconds(10));
                Assert.Equal(0UL, (await box.StatusAsync()).PendingRecords);
            }
            var query = new ProcessStartInfo(PythonExecutable) { RedirectStandardOutput = true };
            query.ArgumentList.Add("-c");
            query.ArgumentList.Add("import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute('SELECT id FROM events').fetchone()[0])");
            query.ArgumentList.Add(database);
            using var check = Process.Start(query)!;
            Assert.Equal(id, (await check.StandardOutput.ReadToEndAsync()).Trim());
            await check.WaitForExitAsync();
            Assert.Equal(0, check.ExitCode);
        }
        finally { directory.Delete(recursive: true); }
    }

    [Fact(Timeout = 30000)]
    public async Task AcknowledgementLogPagesWaitsAndWatches()
    {
        var directory = Directory.CreateTempSubdirectory("aster-audit-dotnet-");
        try
        {
            using var receiver = new Receiver(Path.Combine(directory.FullName, "receiver.sqlite"));
            using var rt = new Runtime();
            await using var box = await AuditOutbox.OpenAsync(rt, new AuditOutboxConfig(Path.Combine(directory.FullName, "outbox"), new(receiver.Endpoint)));
            Assert.Empty(await box.AcknowledgementsAsync());
            // Nothing follows: a short wait returns an empty page.
            Assert.Empty(await box.WaitAcknowledgementsAsync(0, 10, TimeSpan.FromMilliseconds(50)));

            using var watchCts = new CancellationTokenSource(TimeSpan.FromSeconds(20));
            var watched = new List<AuditAcknowledgement>();
            var watch = Task.Run(async () =>
            {
                await foreach (var ack in box.WatchAcknowledgementsAsync(0, watchCts.Token))
                {
                    watched.Add(ack);
                    if (watched.Count == 2) watchCts.Cancel();
                }
            });

            string first = await box.SubmitAsync(new(1, "dotnet.ack", JsonSerializer.SerializeToElement(new { n = 1 })));
            string second = await box.SubmitAsync(new(2, "dotnet.ack", JsonSerializer.SerializeToElement(new { n = 2 })));
            await box.FlushAsync(TimeSpan.FromSeconds(10));

            var page = await box.AcknowledgementsAsync(0, 1000);
            Assert.Equal(new[] { first, second }, page.Select(a => a.EventId));
            Assert.True(page[0].Cursor < page[1].Cursor);
            Assert.All(page, a => { Assert.NotEmpty(a.BatchId); Assert.True(a.AcknowledgedAt > DateTimeOffset.UnixEpoch); });
            Assert.Equal(new[] { second }, (await box.AcknowledgementsAsync(page[0].Cursor, 1)).Select(a => a.EventId));
            Assert.Empty(await box.AcknowledgementsAsync(page[1].Cursor));

            await watch.WaitAsync(TimeSpan.FromSeconds(20));
            Assert.Equal(new[] { first, second }, watched.Select(a => a.EventId));

            foreach (int bad in new[] { 0, 1001 })
                Assert.Equal(AuditErrorKind.InvalidEvent, (await Assert.ThrowsAsync<AuditOutboxException>(() => box.AcknowledgementsAsync(0, bad))).Kind);
            await Assert.ThrowsAsync<AuditOutboxException>(() => box.WaitAcknowledgementsAsync(0, 1, TimeSpan.FromHours(2)));
        }
        finally { directory.Delete(recursive: true); }
    }

    [Fact]
    public void AcknowledgementPageParsesAndRefusesOtherVersions()
    {
        var page = AuditOutbox.ParseAcknowledgements("""{"version":1,"acknowledgements":[{"cursor":7,"event_id":"e","sequence":3,"batch_id":"b","acknowledged_at_ms":1700000000000}]}"""u8);
        var ack = Assert.Single(page);
        Assert.Equal((7UL, "e", 3UL, "b"), (ack.Cursor, ack.EventId, ack.Sequence, ack.BatchId));
        Assert.Equal(DateTimeOffset.FromUnixTimeMilliseconds(1_700_000_000_000), ack.AcknowledgedAt);
        Assert.Throws<IrohException>(() => AuditOutbox.ParseAcknowledgements("""{"version":2,"acknowledgements":[]}"""u8));
    }

    [Fact(Timeout = 20000)]
    public async Task CanceledCloseStillStopsClonesAndReleasesLock()
    {
        var directory = Directory.CreateTempSubdirectory("aster-audit-dotnet-");
        try
        {
            using var rt = new Runtime();
            var config = new AuditOutboxConfig(Path.Combine(directory.FullName, "outbox"), new("http://127.0.0.1:1/audit"));
            using var box = await AuditOutbox.OpenAsync(rt, config);
            using var clone = box.Clone();
            using var stopped = new CancellationTokenSource(); stopped.Cancel();
            try { await box.CloseAsync(stopped.Token); } catch (OperationCanceledException) { }
            await Task.WhenAll(box.CloseAsync(), box.CloseAsync());
            AuditOutbox? reopened = null;
            var deadline = Stopwatch.StartNew();
            while (reopened is null && deadline.Elapsed < TimeSpan.FromSeconds(5))
            {
                try { reopened = await AuditOutbox.OpenAsync(rt, config); }
                catch (AuditOutboxException) { await Task.Delay(10); }
            }
            Assert.NotNull(reopened);
            await reopened!.DisposeAsync();
            var error = await Assert.ThrowsAsync<AuditOutboxException>(() => clone.SubmitAsync(new(0, "closed", JsonSerializer.SerializeToElement(new {}))));
            Assert.Equal(AuditErrorKind.Closed, error.Kind);
        }
        finally { directory.Delete(recursive: true); }
    }

    [Fact]
    public void DestinationDiagnosticsNeverPrintSecrets()
    {
        var destination = new AuditDestination("https://example.test/audit?token=secret", Authorization: "Bearer credential");
        Assert.DoesNotContain("secret", destination.ToString());
        Assert.DoesNotContain("credential", destination.ToString());
    }

    [Theory]
    [InlineData("x")]
    [InlineData("é")]
    public void OversizedPayloadSerializationAllocationRemainsBounded(string character)
    {
        // The caller already owns this document. The wrapper must not make a
        // full raw-text or serialized copy before rejecting its size.
        var input = new AuditEvent(0, "large", JsonSerializer.SerializeToElement(new { data = string.Concat(Enumerable.Repeat(character, 2 * 1024 * 1024)) }));
        AuditOutbox.SerializeRequest("event", new AuditEvent(0, "warmup", JsonSerializer.SerializeToElement(new {})), 64 * 1024 + 1024, AuditErrorKind.EventTooLarge);
        long before = GC.GetAllocatedBytesForCurrentThread();
        var error = Assert.Throws<AuditOutboxException>(() => AuditOutbox.SerializeRequest("event", input, 64 * 1024 + 1024, AuditErrorKind.EventTooLarge));
        long allocated = GC.GetAllocatedBytesForCurrentThread() - before;
        Assert.Equal(AuditErrorKind.EventTooLarge, error.Kind);
        Assert.True(allocated < 1024 * 1024, $"Oversized payload allocated {allocated} bytes while serializing");
    }

    [Fact]
    public void EncodedRequestCeilingIncludesEscapingAndFraming()
    {
        var input = new AuditEvent(0, "expanded", JsonSerializer.SerializeToElement(new { data = new string('<', 13 * 1024) }));
        var error = Assert.Throws<AuditOutboxException>(() => AuditOutbox.SerializeRequest("event", input, 64 * 1024 + 1024, AuditErrorKind.EventTooLarge));
        Assert.Equal(AuditErrorKind.EventTooLarge, error.Kind);
        var encoded = AuditOutbox.SerializeRequest("event", new AuditEvent(ulong.MaxValue, "normal", JsonSerializer.SerializeToElement(new { value = 42 })), 2048, AuditErrorKind.EventTooLarge);
        using var document = JsonDocument.Parse(encoded);
        Assert.Equal(1, document.RootElement.GetProperty("version").GetInt32());
        Assert.Equal(ulong.MaxValue, document.RootElement.GetProperty("event").GetProperty("timestamp_ms").GetUInt64());
    }

    [Fact(Timeout = 20000)]
    public async Task BoundedControlFieldsFailBeforeNativeAdmissionAndPreservePendingData()
    {
        var directory = Directory.CreateTempSubdirectory("aster-audit-dotnet-");
        try
        {
            using var rt = new Runtime();
            await using var box = await AuditOutbox.OpenAsync(rt, new(Path.Combine(directory.FullName, "outbox"), new("http://127.0.0.1:1/audit")));
            string id = await box.SubmitAsync(new(0, "original", JsonSerializer.SerializeToElement(new {})));
            string large = new('x', 2 * 1024 * 1024);
            var error = await Assert.ThrowsAsync<AuditOutboxException>(() => box.QuarantineAsync(id, large, "operator"));
            Assert.Equal(AuditErrorKind.InvalidEvent, error.Kind);
            error = await Assert.ThrowsAsync<AuditOutboxException>(() => box.ReconfigureAsync(new("http://127.0.0.1:1/audit", Authorization: large)));
            Assert.Equal(AuditErrorKind.InvalidConfig, error.Kind);
            error = await Assert.ThrowsAsync<AuditOutboxException>(() => box.ExportAsync(large));
            Assert.Equal(AuditErrorKind.IntegrityConflict, error.Kind);
            error = await Assert.ThrowsAsync<AuditOutboxException>(() => box.ReplayAsync(large));
            Assert.Equal(AuditErrorKind.IntegrityConflict, error.Kind);
            Assert.Equal(1UL, (await box.StatusAsync()).PendingRecords);
            Assert.Equal(0, rt.PendingOperationCount);
        }
        finally { directory.Delete(recursive: true); }
    }
}

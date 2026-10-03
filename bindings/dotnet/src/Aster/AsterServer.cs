using System;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// High-level Aster RPC server. Creates a node, attaches a reactor,
/// and runs a poll loop dispatching calls to a handler.
/// </summary>
public sealed class AsterServer : IDisposable
{
    public const string AsterAlpn = "aster/1";

    private readonly Node _node;
    private readonly Reactor _reactor;
    private readonly Func<ReactorCall, ReactorResponse> _handler;
    private readonly Thread _pollThread;
    private volatile bool _running = true;

    private AsterServer(Node node, Reactor reactor, Func<ReactorCall, ReactorResponse> handler)
    {
        _node = node;
        _reactor = reactor;
        _handler = handler;
        _pollThread = new Thread(PollLoop) { Name = "aster-server-poll", IsBackground = true };
        _pollThread.Start();
    }

    /// <summary>The server's node ID (hex string).</summary>
    public string NodeId() => _node.NodeId();

    /// <summary>The underlying Iroh node.</summary>
    public Node Node => _node;

    /// <summary>Stop the server.</summary>
    public void Dispose()
    {
        _running = false;
        _pollThread.Join(2000);
        _reactor.Dispose();
        _node.Dispose();
    }

    /// <summary>
    /// Raised (on the poll thread) when the reactor reports a connection closed.
    /// Per-connection/session state keyed on <see cref="ReactorCall.ConnectionId"/>
    /// can be dropped here. Handlers must not block.
    /// </summary>
    public event Action<ReactorCall>? ConnectionClosed;

    private void PollLoop()
    {
        while (_running)
        {
            ReactorCall[] calls;
            try { calls = _reactor.Poll(32, 100); }
            catch (ObjectDisposedException) { return; }
            catch (Exception ex)
            {
                Console.Error.WriteLine($"aster reactor poll failed: {ex.Message}");
                Thread.Sleep(10);
                continue;
            }

            foreach (var call in calls)
            {
                if (call.Kind == ReactorEventKind.ConnectionClosed)
                {
                    try { ConnectionClosed?.Invoke(call); }
                    catch (Exception ex) { Console.Error.WriteLine($"ConnectionClosed handler failed: {ex.Message}"); }
                    continue;
                }
                if (call.Kind != ReactorEventKind.Call)
                    continue;

                var c = call;
                // Submits run off the poll thread: a QUEUE_FULL submit waits for a
                // RESPONSE_WRITABLE event that only this loop can deliver.
                ThreadPool.QueueUserWorkItem(_ =>
                {
                    ReactorResponse resp;
                    try
                    {
                        resp = _handler(c);
                    }
                    catch (Exception ex)
                    {
                        var errorBytes = Encoding.UTF8.GetBytes($"ERROR: {ex.Message}");
                        resp = ReactorResponse.Of(Array.Empty<byte>(), errorBytes);
                    }
                    try
                    {
                        _reactor.Submit(c.CallId, resp);
                    }
                    catch (Exception ex)
                    {
                        // e.g. NOT_FOUND: the peer cancelled/disconnected, or the
                        // response channel closed while we waited for capacity.
                        Console.Error.WriteLine($"aster submit for call {c.CallId} failed: {ex.Message}");
                    }
                });
            }
        }
    }

    /// <summary>
    /// Creates an AsterServer with the given handler.
    /// </summary>
    public static async Task<AsterServer> CreateAsync(
        Func<ReactorCall, ReactorResponse> handler,
        uint ringCapacity = 256,
        CancellationToken cancellationToken = default)
    {
        var node = await Node.MemoryWithAlpnsAsync(new[] { AsterAlpn }, cancellationToken).ConfigureAwait(false);
        try
        {
            var reactor = new Reactor(node.Runtime.Handle, node.Handle.Value, ringCapacity);
            return new AsterServer(node, reactor, handler);
        }
        catch
        {
            node.Dispose();
            throw;
        }
    }

    // ─── Framing ─────────────────────────────────────────────────────────────

    public const byte FlagHeader = 0x04;

    /// <summary>
    /// Encodes a payload with flags into the Aster wire format.
    /// </summary>
    public static byte[] EncodeFrame(byte[] payload, byte flags)
    {
        int frameBodyLen = 1 + payload.Length;
        byte[] frame = new byte[4 + frameBodyLen];
        BitConverter.TryWriteBytes(frame.AsSpan(0, 4), (uint)frameBodyLen);
        frame[4] = flags;
        Buffer.BlockCopy(payload, 0, frame, 5, payload.Length);
        return frame;
    }
}

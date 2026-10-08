using Apache.Fory;
using Aster.Codec;
using Aster.Rpc;

namespace Aster.Tests;

[ForyStruct]
[WireType("test/ConcurrentCodecPayload")]
public sealed class ConcurrentCodecPayload
{
    public string Message { get; set; } = "";
    public int Count { get; set; }
    public byte[] Data { get; set; } = [];
}

public sealed class ForyConcurrencyTests
{
    [Fact(Timeout = 30000)]
    public async Task Concurrent_FirstDecode_PreservesEveryFieldAcrossCodecs()
    {
        const int workers = 16;
        const int rounds = 128;
        using var start = new Barrier(workers);
        var tasks = Enumerable.Range(0, workers).Select(worker => Task.Factory.StartNew(() =>
        {
            using var codec = new ForyCodec().Register<ConcurrentCodecPayload>("test/ConcurrentCodecPayload");
            // Separate clients/servers register different service type sets,
            // while sharing this payload's generated serializer.
            if (worker % 2 != 0)
                codec.Register<EchoRequest>("test/AdditionalServiceRequest");
            for (int round = 0; round < rounds; round++)
            {
                var expected = new ConcurrentCodecPayload
                {
                    Message = $"worker-{worker}-round-{round}",
                    Count = worker * rounds + round + 1,
                    Data = [(byte)worker, (byte)round],
                };
                // Encode prepares wire bytes and write-side schema hashes;
                // no Decode runs before the first rendezvous, so every
                // worker's first read-side metadata lookup is still cold.
                byte[] encoded = codec.Encode(expected);
                Assert.True(start.SignalAndWait(TimeSpan.FromSeconds(10)), "codec workers did not rendezvous");
                var actual = codec.Decode<ConcurrentCodecPayload>(encoded);
                Assert.Equal(expected.Count, actual.Count);
                Assert.Equal(expected.Message, actual.Message);
                Assert.Equal(expected.Data, actual.Data);
            }
        }, CancellationToken.None, TaskCreationOptions.LongRunning, TaskScheduler.Default)).ToArray();
        await Task.WhenAll(tasks);
    }
}

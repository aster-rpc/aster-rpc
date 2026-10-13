using System;
using System.Linq;
using System.Threading.Tasks;
using Aster;

namespace Examples;

class Program
{
    static async Task<int> Main(string[] args)
    {
        string command = args.Length > 0 ? args[0] : "echo";
        string[] rest = args.Skip(1).ToArray();
        switch (command)
        {
            case "runtime":
                RunRuntimeTest();
                return 0;
            case "mc-server":
                return await MissionControl.ServerAsync(rest);
            case "mc-agent":
                return await MissionControl.AgentAsync(rest);
            case "echo":
                await AsterServerExample.RunAsync();
                return 0;
            default:
                Console.Error.WriteLine("usage: Examples [echo | runtime | mc-server [--auth] [--config aster.toml] | mc-agent <address>]");
                return 2;
        }
    }

    static void RunRuntimeTest()
    {
        Console.WriteLine("=== Aster .NET Runtime Test ===\n");

        var (major, minor, patch) = Runtime.Version;
        Console.WriteLine($"ABI Version: {major}.{minor}.{patch}");

        Console.WriteLine("\n1. Creating runtime...");
        using var runtime = new Runtime();
        Console.WriteLine($"   Runtime created! Handle: {runtime.Handle}");

        Console.WriteLine("\n2. Creating second runtime...");
        using var runtime2 = new Runtime();
        Console.WriteLine($"   Runtime 2 created! Handle: {runtime2.Handle}");

        runtime2.Dispose();
        Console.WriteLine("   Runtime 2 closed.");

        Console.WriteLine("\n=== SUCCESS: Runtime init works! ===");
    }
}

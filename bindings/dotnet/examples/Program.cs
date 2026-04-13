using System;
using System.Threading.Tasks;
using Aster;

namespace Examples;

class Program
{
    static async Task Main(string[] args)
    {
        if (args.Length > 0 && args[0] == "runtime")
        {
            RunRuntimeTest();
            return;
        }

        // Default: run the AsterServer echo example
        await AsterServerExample.RunAsync();
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

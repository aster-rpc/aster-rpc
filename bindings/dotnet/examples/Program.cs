using System;
using Aster;

namespace Examples;

class Program
{
    static void Main(string[] args)
    {
        Console.WriteLine("=== Aster .NET Runtime Test ===\n");

        // ABI version
        var (major, minor, patch) = Runtime.Version;
        Console.WriteLine($"ABI Version: {major}.{minor}.{patch}");

        // Create runtime
        Console.WriteLine("\n1. Creating runtime...");
        using var runtime = new Runtime();
        Console.WriteLine($"   Runtime created! Handle: {runtime.Handle}");

        // Create a second runtime to verify multiple runtimes work
        Console.WriteLine("\n2. Creating second runtime...");
        using var runtime2 = new Runtime();
        Console.WriteLine($"   Runtime 2 created! Handle: {runtime2.Handle}");

        // Close second runtime
        runtime2.Dispose();
        Console.WriteLine("   Runtime 2 closed.");

        Console.WriteLine("\n=== SUCCESS: Runtime init works! ===");
    }
}

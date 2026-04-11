using System;

namespace Aster;

/// <summary>
/// Thrown when an iroh FFI call returns a non-zero status code.
/// </summary>
public class IrohException : Exception
{
    public Status Status { get; }

    public IrohException(Status status, string message) : base(message)
    {
        Status = status;
    }

    public IrohException(string message) : base(message)
    {
        Status = Status.Internal;
    }

    public static IrohException FromStatus(int code, string context)
    {
        var status = (Status)code;
        string name = Native.iroh_status_name((uint)code) != IntPtr.Zero
            ? Marshal.PtrToStringAnsi(Native.iroh_status_name((uint)code)) ?? status.ToString()
            : status.ToString();
        return new IrohException(status, $"{context}: {name} ({code})");
    }
}

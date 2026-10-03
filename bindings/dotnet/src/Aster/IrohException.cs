using System;
using System.Runtime.InteropServices;

namespace Aster;

/// <summary>
/// Thrown when an Aster FFI call returns a non-zero status code.
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
        IntPtr namePtr = Native.aster_status_name(code);
        string name = namePtr != IntPtr.Zero
            ? Marshal.PtrToStringUTF8(namePtr) ?? status.ToString()
            : status.ToString();
        string? detail = LastErrorMessage();
        string msg = string.IsNullOrEmpty(detail)
            ? $"{context}: {name} ({code})"
            : $"{context}: {name} ({code}): {detail}";
        return new IrohException(status, msg);
    }

    /// <summary>Thread-local last error message from the native library, if any.</summary>
    internal static unsafe string? LastErrorMessage()
    {
        Span<byte> buf = stackalloc byte[512];
        nuint n;
        fixed (byte* p = buf)
            n = (nuint)Native.aster_last_error_message(p, (UIntPtr)buf.Length);
        if (n == 0) return null;
        int len = (int)Math.Min(n, (nuint)buf.Length);
        // The native side may NUL-terminate inside the buffer.
        int nul = buf[..len].IndexOf((byte)0);
        if (nul >= 0) len = nul;
        return System.Text.Encoding.UTF8.GetString(buf[..len]);
    }
}

using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

namespace Aster;

/// <summary>
/// Owns unmanaged copies of the byte buffers, lists and structs handed to one native call
/// (or one async operation). Everything is freed on <see cref="Dispose"/>, so a
/// <see cref="Bytes"/> / <see cref="BytesList"/> built here stays valid — and never moves —
/// until the scope is disposed. This replaces ad-hoc <c>GCHandle</c> pinning.
/// </summary>
internal sealed unsafe class NativeScope : IDisposable
{
    private readonly List<IntPtr> _allocations = new();
    private List<Action>? _cleanups;

    /// <summary>Runs <paramref name="cleanup"/> on <see cref="Dispose"/>, before the allocations are freed (latest first).</summary>
    public void OnDispose(Action cleanup) => (_cleanups ??= new()).Add(cleanup);

    /// <summary>Allocates <paramref name="size"/> zeroed bytes owned by this scope.</summary>
    public IntPtr Alloc(int size)
    {
        IntPtr p = (IntPtr)NativeMemory.AllocZeroed((nuint)Math.Max(size, 1));
        _allocations.Add(p);
        return p;
    }

    /// <summary>A pointer to a zeroed <typeparamref name="T"/> owned by this scope.</summary>
    public T* Struct<T>() where T : unmanaged => (T*)Alloc(sizeof(T));

    /// <summary>An unmanaged copy of <paramref name="data"/> (<c>{NULL,0}</c> when empty).</summary>
    public Bytes Bytes(ReadOnlySpan<byte> data)
    {
        if (data.IsEmpty) return default;
        IntPtr p = Alloc(data.Length);
        data.CopyTo(new Span<byte>((void*)p, data.Length));
        return new Bytes { ptr = p, len = (UIntPtr)data.Length };
    }

    /// <summary>UTF-8 bytes of <paramref name="s"/> (<c>{NULL,0}</c> when null or empty).</summary>
    public Bytes Utf8(string? s) => string.IsNullOrEmpty(s) ? default : Bytes(Encoding.UTF8.GetBytes(s));

    /// <summary>A pointer to an unmanaged copy of <paramref name="data"/> (for <c>const uint8_t*</c> args).</summary>
    public byte* Ptr(ReadOnlySpan<byte> data) => (byte*)Bytes(data).ptr;

    /// <summary>An <c>aster_bytes_list_t</c> over unmanaged copies of <paramref name="items"/>.</summary>
    public BytesList List(IReadOnlyList<byte[]> items)
    {
        if (items.Count == 0) return default;
        var arr = (Bytes*)Alloc(sizeof(Bytes) * items.Count);
        for (int i = 0; i < items.Count; i++) arr[i] = Bytes(items[i]);
        return new BytesList { items = (IntPtr)arr, len = (UIntPtr)items.Count };
    }

    /// <summary>An <c>aster_bytes_list_t</c> of UTF-8 strings.</summary>
    public BytesList Utf8List(IReadOnlyList<string> items)
    {
        var bytes = new byte[items.Count][];
        for (int i = 0; i < items.Count; i++) bytes[i] = Encoding.UTF8.GetBytes(items[i]);
        return List(bytes);
    }

    public void Dispose()
    {
        if (_cleanups is { } cleanups)
        {
            _cleanups = null;
            for (int i = cleanups.Count - 1; i >= 0; i--) cleanups[i]();
        }
        foreach (var p in _allocations) NativeMemory.Free((void*)p);
        _allocations.Clear();
    }

    // ── reading native memory ─────────────────────────────────────────────

    /// <summary>Copies a borrowed <c>aster_bytes_t</c>.</summary>
    public static byte[] Read(Bytes b) => b.ToArray();

    /// <summary>Copies a borrowed <c>aster_bytes_t</c> as UTF-8 text.</summary>
    public static string ReadUtf8(Bytes b) => b.ToUtf8String();

    /// <summary>Copies a borrowed <c>aster_bytes_list_t</c> of UTF-8 strings.</summary>
    public static List<string> ReadUtf8List(BytesList l)
    {
        var result = new List<string>((int)l.len);
        if (l.items == IntPtr.Zero) return result;
        var arr = (Bytes*)l.items;
        for (nuint i = 0; i < (nuint)l.len; i++) result.Add(arr[i].ToUtf8String());
        return result;
    }

    /// <summary>
    /// Copies a library-owned output buffer and frees it with <c>aster_string_release</c>.
    /// </summary>
    public static byte[] TakeOwned(ref Bytes b)
    {
        if (b.ptr == IntPtr.Zero) return Array.Empty<byte>();
        try { return b.ToArray(); }
        finally
        {
            Native.aster_string_release(b.ptr, b.len);
            b = default;
        }
    }
}

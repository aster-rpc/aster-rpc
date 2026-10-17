using System.Reflection;
using System.Text;

namespace Aster.Rpc;

/// <summary>
/// Field names (<c>ffi_spec/Aster-FieldNaming.md</c>). A field's contract name is the C# member
/// name, verbatim, or its <see cref="WireNameAttribute"/> when the contract is defined in another
/// language. Fory C# matches fields by its snake_case form of the member name, so contract
/// definition refuses a name that is not plain, a member whose Fory form differs from the
/// reference snake_case form of its name, and two fields with the same snake_case form.
/// </summary>
public static class FieldNames
{
    /// <summary>A member's contract name: its <see cref="WireNameAttribute"/>, else its name.</summary>
    public static string ContractName(MemberInfo member)
        => member.GetCustomAttribute<WireNameAttribute>()?.Name ?? member.Name;

    /// <summary>
    /// Fory C#'s snake_case form of a member name (<c>TypeMetaUtils.LowerCamelToLowerUnderscore</c>
    /// and the source generator's <c>ToSnakeCase</c> in Apache.Fory 1.7.6): Rust's rule with
    /// Unicode <see cref="char.IsUpper(char)"/>.
    /// </summary>
    public static string ForySnakeCase(string name)
    {
        var sb = new StringBuilder(name.Length + 4);
        for (int i = 0; i < name.Length; i++)
        {
            char c = name[i];
            if (char.IsUpper(c))
            {
                if (i > 0)
                {
                    bool prevUpper = char.IsUpper(name[i - 1]);
                    bool nextUpperOrEnd = i + 1 >= name.Length || char.IsUpper(name[i + 1]);
                    if (!prevUpper || !nextUpperOrEnd) sb.Append('_');
                }
                sb.Append(char.ToLowerInvariant(c));
            }
            else sb.Append(c);
        }
        return sb.ToString();
    }

    /// <summary>The reference snake_case form (Rust fory-core / Fory JavaScript): ASCII capitals only.</summary>
    public static string ReferenceSnakeCase(string name)
    {
        var sb = new StringBuilder(name.Length + 4);
        for (int i = 0; i < name.Length; i++)
        {
            char c = name[i];
            if (IsAsciiUpper(c))
            {
                if (i > 0)
                {
                    bool prevUpper = IsAsciiUpper(name[i - 1]);
                    bool nextUpperOrEnd = i + 1 >= name.Length || IsAsciiUpper(name[i + 1]);
                    if (!prevUpper || !nextUpperOrEnd) sb.Append('_');
                }
                sb.Append((char)(c + 32));
            }
            else sb.Append(c);
        }
        return sb.ToString();
    }

    private static bool IsAsciiUpper(char c) => c is >= 'A' and <= 'Z';
    private static bool IsAsciiLowerOrDigit(char c) => c is >= 'a' and <= 'z' or >= '0' and <= '9';

    /// <summary>
    /// Why <paramref name="name"/> is not plain, or null when it is: every ASCII capital starts the
    /// name or follows an ASCII lowercase letter or a digit, no non-ASCII letter is uppercase, and
    /// the name does not end in <c>_</c>.
    /// </summary>
    public static string? NotPlain(string name)
    {
        var runes = name.EnumerateRunes().ToArray();
        for (int i = 0; i < runes.Length; i++)
        {
            var r = runes[i];
            if (r.IsAscii && IsAsciiUpper((char)r.Value) && i > 0
                && !(runes[i - 1].IsAscii && IsAsciiLowerOrDigit((char)runes[i - 1].Value)))
                return "an uppercase letter follows another uppercase letter, `_` or a non-ASCII character";
            if (!r.IsAscii && Rune.IsUpper(r)) return "it has an uppercase letter outside ASCII";
        }
        return name.EndsWith('_') ? "it ends in `_`" : null;
    }

    /// <summary>A plain spelling of <paramref name="name"/> (<c>userID</c> → <c>userId</c>, <c>HTTPServer</c> → <c>HttpServer</c>).</summary>
    public static string PlainSuggestion(string name)
    {
        var rs = name.EnumerateRunes().ToArray();
        int i = 0;
        while (i < rs.Length)
        {
            if (!(rs[i].IsAscii && IsAsciiUpper((char)rs[i].Value)))
            {
                if (!rs[i].IsAscii && Rune.IsUpper(rs[i])) rs[i] = Rune.ToLowerInvariant(rs[i]);
                i++;
                continue;
            }
            int end = i;
            while (end < rs.Length && rs[end].IsAscii && IsAsciiUpper((char)rs[end].Value)) end++;
            bool nextLower = end < rs.Length && rs[end].IsAscii && rs[end].Value is >= 'a' and <= 'z';
            bool keepLast = nextLower && end - i >= 3;
            bool firstOk = i == 0 || (rs[i - 1].IsAscii && IsAsciiLowerOrDigit((char)rs[i - 1].Value));
            for (int k = i; k < end; k++)
                if (!((k == i && firstOk) || (k == end - 1 && keepLast))) rs[k] = Rune.ToLowerInvariant(rs[k]);
            i = end;
        }
        return string.Concat(rs.Select(r => r.ToString())).TrimEnd('_');
    }

    /// <summary>
    /// Check one type's fields, each as (Fory's snake_case form of the member, contract name).
    /// Throws <see cref="ArgumentException"/> naming the first offending field.
    /// </summary>
    public static void Check(IReadOnlyList<(string MemberSnake, string Name)> fields)
    {
        foreach (var (memberSnake, name) in fields)
        {
            if (NotPlain(name) is { } why)
                throw new ArgumentException(
                    $"\"{name}\" is not a plain field name: {why}, so the Fory implementations disagree on its "
                    + $"snake_case form. Use a plain spelling such as \"{PlainSuggestion(name)}\" (ffi_spec/Aster-FieldNaming.md §5)");
            string snake = ReferenceSnakeCase(name);
            if (snake != memberSnake)
                throw new ArgumentException(
                    $"the field name \"{name}\" does not match its member: Fory writes the member as \"{memberSnake}\" "
                    + $"but \"{name}\" is \"{snake}\". Name the member so its snake_case form is \"{snake}\", with "
                    + $"[WireName(\"{name}\")] (ffi_spec/Aster-FieldNaming.md §4)");
        }
        for (int i = 0; i < fields.Count; i++)
            for (int j = 0; j < i; j++)
                if (ReferenceSnakeCase(fields[j].Name) == ReferenceSnakeCase(fields[i].Name))
                    throw new ArgumentException(
                        $"the fields \"{fields[j].Name}\" and \"{fields[i].Name}\" collide: Fory writes both as "
                        + $"\"{ReferenceSnakeCase(fields[i].Name)}\" and could not tell them apart (ffi_spec/Aster-FieldNaming.md §4)");
    }

    /// <summary>Check a type's payload members (contract definition).</summary>
    internal static void Check(Type type, IEnumerable<MemberInfo> members)
    {
        try
        {
            Check(members.Select(m => (ForySnakeCase(m.Name), ContractName(m))).ToList());
        }
        catch (ArgumentException e)
        {
            throw new ArgumentException($"{type.FullName}: {e.Message}", e);
        }
    }
}

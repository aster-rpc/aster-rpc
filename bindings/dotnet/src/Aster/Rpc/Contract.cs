using System;
using System.Collections;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Reflection;
using System.Text;
using System.Text.Json;

namespace Aster.Rpc;

/// <summary>
/// The Aster wire identity of a payload type: <c>"namespace/Name"</c>, used both as the Fory
/// registration name and as the contract <c>TypeDef</c> (package, name). Apply it to every
/// <c>[ForyStruct]</c> request/response class and enum of a service.
/// </summary>
[AttributeUsage(AttributeTargets.Class | AttributeTargets.Struct | AttributeTargets.Enum, Inherited = false)]
public sealed class WireTypeAttribute(string tag) : Attribute
{
    public string Tag { get; } = tag;

    internal (string Package, string Name) Split()
    {
        int slash = Tag.LastIndexOf('/');
        return slash >= 0 ? (Tag[..slash], Tag[(slash + 1)..]) : (string.Empty, Tag);
    }
}

/// <summary>
/// Overrides a property's contract/wire field name (default: the property name in snake_case,
/// which is also what Fory writes).
/// </summary>
[AttributeUsage(AttributeTargets.Property | AttributeTargets.Field, Inherited = false)]
public sealed class WireNameAttribute(string name) : Attribute
{
    public string Name { get; } = name;
}

/// <summary>
/// Declares that a field's construction default is <b>part of the contract</b> (the counterpart of
/// Rust's <c>#[aster(default = ...)]</c> and Python's <c>contract_default()</c>): the field is
/// published as <c>required = false</c> plus the canonical default bytes, and the contract id
/// changes with the default. A plain initializer (<c>public string Name { get; set; } = "";</c>)
/// does <i>not</i> enter the contract: the field stays <c>required</c>. Fory payload bytes are
/// unaffected.
/// <code>
/// [WireType("defaults.v1/Settings")]
/// public class Settings
/// {
///     public string Name { get; set; } = "";                   // required
///     [ContractDefault] public string State { get; set; } = "idle";
///     [ContractDefault] public int Retries { get; set; } = -3;
///     [ContractDefault] public List&lt;string&gt; Tags { get; set; } = new();
/// }
/// </code>
/// The default is the value a freshly constructed instance holds (the type needs a public
/// parameterless constructor, as Fory requires anyway). Supported (§11.3.2.3): <c>string</c>,
/// <c>bool</c>, <c>int</c> (varint32), <c>long</c> (varint64), <c>double</c> (float64; finite,
/// not subnormal), nullable forms of those with a non-null value, and the <i>empty</i>
/// list / set / map. Anything else throws <see cref="NotSupportedException"/> when the contract
/// is computed.
/// </summary>
[AttributeUsage(AttributeTargets.Property | AttributeTargets.Field, Inherited = false)]
public sealed class ContractDefaultAttribute : Attribute;

/// <summary>
/// Contract identity (Aster-ContractIdentity §11.3): builds the <c>TypeDef</c> /
/// <c>ServiceContract</c> JSON from a <see cref="ServiceDefinition"/> and lets Rust canonicalize
/// and hash it (<c>aster_canonical_bytes</c>, <c>aster_blake3_hex</c>, <c>aster_contract_id</c>);
/// no canonical bytes or hashes are computed in .NET.
///
/// .NET → contract primitive: <c>string</c> string, <c>bool</c> bool, <c>sbyte</c> int8,
/// <c>short</c> int16, <c>int</c> varint32, <c>long</c> varint64, <c>byte</c> uint8,
/// <c>ushort</c> uint16, <c>uint</c> var_uint32, <c>ulong</c> var_uint64, <c>float</c> float32,
/// <c>double</c> float64, <c>byte[]</c> binary (the same table as Rust and Java). Lists/arrays,
/// sets and dictionaries become containers; <see cref="WireTypeAttribute"/> classes and enums
/// become refs. Nullable value types and <c>T?</c> references are optional.
/// </summary>
public static class ContractBuilder
{
    /// <summary>The 64-hex contract id of <paramref name="service"/>.</summary>
    public static string ComputeContractId(ServiceDefinition service)
        => ContractIdentity.ComputeContractId(ServiceContractJson(service));

    /// <summary>The <c>ServiceContract</c> JSON handed to <c>aster_contract_id</c>.</summary>
    public static string ServiceContractJson(ServiceDefinition service)
    {
        var resolver = new Resolver();
        using var ms = new MemoryStream();
        using (var w = new Utf8JsonWriter(ms))
        {
            w.WriteStartObject();
            w.WriteString("name", service.Name);
            w.WriteNumber("version", service.Version);
            w.WriteStartArray("methods");
            foreach (var m in service.OrderedMethods)
            {
                w.WriteStartObject();
                w.WriteString("name", m.Name);
                w.WriteString("pattern", PatternWire(m.Pattern));
                w.WriteString("request_type", resolver.Hash(m.RequestType));
                w.WriteString("response_type", resolver.Hash(m.ResponseType));
                w.WriteBoolean("idempotent", m.Idempotent);
                w.WriteNumber("default_timeout", m.DefaultTimeout?.TotalSeconds ?? 0.0);
                WriteRequires(w, m.Requires);
                w.WriteEndObject();
            }
            w.WriteEndArray();
            w.WriteStartArray("serialization_modes");
            w.WriteStringValue("xlang");
            w.WriteEndArray();
            w.WriteString("scoped", service.Scope == ServiceScope.Session ? "session" : "shared");
            WriteRequires(w, service.Requires);
            w.WriteString("producer_language", "");
            w.WriteEndObject();
        }
        return Encoding.UTF8.GetString(ms.ToArray());
    }

    /// <summary>
    /// The JSON registration form (<c>DynamicService</c>, dynamic-dispatch design §1.5) for
    /// <c>aster_server_add_service_json</c>. Methods are in <see cref="Ordered"/> order, so a
    /// method's index is its dispatch <c>method_idx</c>. Rust derives the same contract id from it
    /// as <see cref="ComputeContractId"/> (checked by <c>aster_dynamic_service_contract_id</c> in
    /// the tests).
    /// </summary>
    public static string RegistrationJson(ServiceDefinition service)
    {
        var resolver = new Resolver();
        using var ms = new MemoryStream();
        using (var w = new Utf8JsonWriter(ms))
        {
            w.WriteStartObject();
            w.WriteString("name", service.Name);
            w.WriteNumber("version", service.Version);
            w.WriteString("scope", service.Scope == ServiceScope.Session ? "session" : "shared");
            if (service.Requires != null) WriteRequires(w, service.Requires);
            w.WriteStartArray("serialization_modes");
            w.WriteStringValue("xlang");
            w.WriteEndArray();
            w.WriteStartArray("methods");
            foreach (var m in service.OrderedMethods)
            {
                w.WriteStartObject();
                w.WriteString("name", m.Name);
                w.WriteString("pattern", PatternWire(m.Pattern));
                w.WriteStartObject("request");
                w.WriteString("hash", resolver.Hash(m.RequestType));
                w.WriteEndObject();
                w.WriteStartObject("response");
                w.WriteString("hash", resolver.Hash(m.ResponseType));
                w.WriteEndObject();
                w.WriteBoolean("idempotent", m.Idempotent);
                if (m.DefaultTimeout is { } t && t > TimeSpan.Zero) w.WriteNumber("default_timeout", t.TotalSeconds);
                if (m.Requires != null) WriteRequires(w, m.Requires);
                w.WriteEndObject();
            }
            w.WriteEndArray();
            w.WriteEndObject();
        }
        return Encoding.UTF8.GetString(ms.ToArray());
    }

    /// <summary>Methods sorted by NFC name, ordinal: the contract (and dispatch index) order.</summary>
    internal static MethodDefinition[] Ordered(IEnumerable<MethodDefinition> methods)
        => methods.OrderBy(m => m.Name.Normalize(NormalizationForm.FormC), StringComparer.Ordinal).ToArray();

    private static string PatternWire(MethodPattern p) => p switch
    {
        MethodPattern.Unary => "unary",
        MethodPattern.ServerStream => "server_stream",
        MethodPattern.ClientStream => "client_stream",
        _ => "bidi_stream",
    };

    private static void WriteRequires(Utf8JsonWriter w, CapabilityRequirement? r)
    {
        if (r is null)
        {
            w.WriteNull("requires");
            return;
        }
        w.WriteStartObject("requires");
        w.WriteString("kind", r.WireKind);
        w.WriteStartArray("roles");
        foreach (var role in r.Roles) w.WriteStringValue(role);
        w.WriteEndArray();
        w.WriteEndObject();
    }

    /// <summary>The <c>TypeDef</c> JSON of a <see cref="WireTypeAttribute"/> class or enum.</summary>
    public static string TypeDefJson(Type type) => new Resolver().TypeDefJson(type);

    /// <summary>The 64-hex type hash: BLAKE3 of the canonical <c>TypeDef</c> bytes.</summary>
    public static string TypeHash(Type type) => new Resolver().Hash(type);

    /// <summary>PascalCase / camelCase → snake_case (what Fory writes for a property name).</summary>
    public static string SnakeCase(string name)
    {
        var sb = new StringBuilder(name.Length + 4);
        for (int i = 0; i < name.Length; i++)
        {
            char c = name[i];
            if (char.IsUpper(c))
            {
                bool prevLowerOrDigit = i > 0 && (char.IsLower(name[i - 1]) || char.IsDigit(name[i - 1]));
                bool acronymEnd = i > 0 && char.IsUpper(name[i - 1]) && i + 1 < name.Length && char.IsLower(name[i + 1]);
                if (sb.Length > 0 && sb[^1] != '_' && (prevLowerOrDigit || acronymEnd)) sb.Append('_');
                sb.Append(char.ToLowerInvariant(c));
            }
            else sb.Append(c);
        }
        return sb.ToString();
    }

    private sealed class Resolver
    {
        private readonly Dictionary<Type, string> _hashes = new();
        private readonly HashSet<Type> _inProgress = new();
        private readonly NullabilityInfoContext _nullability = new();
        private readonly Dictionary<Type, object> _defaultInstances = new();

        public string Hash(Type type)
        {
            if (_hashes.TryGetValue(type, out var h)) return h;
            if (!_inProgress.Add(type))
                throw new NotSupportedException($"recursive type graphs are not supported yet ({type.Name})");
            try
            {
                string json = TypeDefJson(type);
                h = Blake3Hex(ContractIdentity.ComputeCanonicalBytes("TypeDef", json));
                _hashes[type] = h;
                return h;
            }
            finally { _inProgress.Remove(type); }
        }

        public string TypeDefJson(Type type)
        {
            var wire = type.GetCustomAttribute<WireTypeAttribute>()
                ?? throw new InvalidOperationException($"{type.FullName} has no [WireType(\"namespace/Name\")] attribute");
            var (package, name) = wire.Split();
            using var ms = new MemoryStream();
            using (var w = new Utf8JsonWriter(ms))
            {
                w.WriteStartObject();
                w.WriteString("kind", type.IsEnum ? "enum" : "message");
                w.WriteString("package", package);
                w.WriteString("name", name);
                w.WriteStartArray("fields");
                if (!type.IsEnum)
                    foreach (var member in Members(type)) WriteField(w, member);
                w.WriteEndArray();
                w.WriteStartArray("enum_values");
                if (type.IsEnum)
                {
                    foreach (var f in type.GetFields(BindingFlags.Public | BindingFlags.Static))
                    {
                        w.WriteStartObject();
                        w.WriteString("name", f.Name);
                        w.WriteNumber("value", Convert.ToInt32(f.GetRawConstantValue()));
                        w.WriteEndObject();
                    }
                }
                w.WriteEndArray();
                w.WriteStartArray("union_variants");
                w.WriteEndArray();
                w.WriteEndObject();
            }
            return Encoding.UTF8.GetString(ms.ToArray());
        }

        private static IEnumerable<MemberInfo> Members(Type type)
        {
            const BindingFlags flags = BindingFlags.Public | BindingFlags.Instance;
            foreach (var p in type.GetProperties(flags))
                if (p.CanRead && p.CanWrite && p.GetIndexParameters().Length == 0 && !Ignored(p)) yield return p;
            foreach (var f in type.GetFields(flags))
                if (!Ignored(f)) yield return f;
        }

        private static bool Ignored(MemberInfo m)
            => m.GetCustomAttribute<Apache.Fory.ForyFieldAttribute>() is { Ignore: true };

        private void WriteField(Utf8JsonWriter w, MemberInfo member)
        {
            Type t = member is PropertyInfo p ? p.PropertyType : ((FieldInfo)member).FieldType;
            bool optional = false;
            if (Nullable.GetUnderlyingType(t) is { } inner) { t = inner; optional = true; }
            else if (!t.IsValueType)
            {
                var info = member is PropertyInfo pi ? _nullability.Create(pi) : _nullability.Create((FieldInfo)member);
                optional = info.ReadState == NullabilityState.Nullable;
            }

            string container = "none";
            Type valueType = t;
            Type? keyType = null;
            if (t != typeof(byte[]) && t != typeof(string))
            {
                if (t.IsArray) { container = "list"; valueType = t.GetElementType()!; }
                else if (Generic(t, typeof(IDictionary<,>)) is { } dict) { container = "map"; keyType = dict[0]; valueType = dict[1]; }
                else if (Generic(t, typeof(ISet<>)) is { } set) { container = "set"; valueType = set[0]; }
                else if (Generic(t, typeof(IEnumerable<>)) is { } list) { container = "list"; valueType = list[0]; }
            }

            var (kind, primitive, typeRef) = Leaf(valueType);
            bool required = true;
            string defaultHex = "";
            if (member.GetCustomAttribute<ContractDefaultAttribute>() != null)
            {
                required = false;
                defaultHex = Convert.ToHexString(LowerDefault(member, container, kind, primitive)).ToLowerInvariant();
            }
            w.WriteStartObject();
            w.WriteNumber("id", 0);
            w.WriteString("name", member.GetCustomAttribute<WireNameAttribute>()?.Name ?? SnakeCase(member.Name));
            w.WriteString("type_kind", kind);
            w.WriteString("type_primitive", primitive);
            w.WriteString("type_ref", typeRef);
            w.WriteString("self_ref_name", "");
            w.WriteBoolean("optional", optional);
            w.WriteBoolean("ref_tracked", false);
            w.WriteString("container", container);
            if (keyType != null)
            {
                var (kk, kp, kr) = Leaf(keyType);
                w.WriteString("container_key_kind", kk);
                w.WriteString("container_key_primitive", kp);
                w.WriteString("container_key_ref", kr);
            }
            else
            {
                w.WriteString("container_key_kind", "primitive");
                w.WriteString("container_key_primitive", "");
                w.WriteString("container_key_ref", "");
            }
            w.WriteBoolean("required", required);
            w.WriteString("default_value", defaultHex);
            w.WriteEndObject();
        }

        /// <summary>
        /// <c>FieldDef.default_value</c> for a <see cref="ContractDefaultAttribute"/> field: the
        /// canonical encoding of the default-constructed value (§11.3.2.3 lowering table).
        /// </summary>
        private byte[] LowerDefault(MemberInfo member, string container, string kind, string primitive)
        {
            Type owner = member.ReflectedType ?? member.DeclaringType!;
            string where = $"{owner.Name}.{member.Name}";
            if (!_defaultInstances.TryGetValue(owner, out var instance))
            {
                try { instance = Activator.CreateInstance(owner)!; }
                catch (Exception e) when (e is MissingMethodException or TargetInvocationException)
                {
                    throw new NotSupportedException($"{where}: [ContractDefault] needs a public parameterless constructor on {owner.Name}", e);
                }
                _defaultInstances[owner] = instance;
            }
            object? value = member is PropertyInfo p ? p.GetValue(instance) : ((FieldInfo)member).GetValue(instance);

            if (container != "none")
            {
                if (value is not IEnumerable items || items.GetEnumerator().MoveNext())
                    throw new NotSupportedException($"{where}: a list/set/map contract default must be the empty container (§11.3.2.3 rule 2)");
                return [0x00];
            }
            if (value is null)
                throw new NotSupportedException($"{where}: a null contract default is not expressible in v1; drop [ContractDefault] to keep the field required (§11.3.2.3)");
            if (kind != "primitive")
                throw new NotSupportedException($"{where}: only scalar fields may carry a contract default (§11.3.2.3 rule 3)");
            switch (primitive)
            {
                case "string":
                {
                    byte[] utf8 = Encoding.UTF8.GetBytes((string)value);
                    return [.. UVarint(((ulong)utf8.Length << 2) | 2), .. utf8];
                }
                case "bool":
                    return [(bool)value ? (byte)1 : (byte)0];
                case "varint32":
                {
                    int v = (int)value;
                    return UVarint((uint)((v << 1) ^ (v >> 31)));
                }
                case "varint64":
                {
                    long v = (long)value;
                    return UVarint((ulong)((v << 1) ^ (v >> 63)));
                }
                case "float64":
                {
                    double d = (double)value;
                    if (!double.IsFinite(d)) throw new NotSupportedException($"{where}: float contract default must be finite");
                    if (d != 0.0 && Math.Abs(d) < 2.2250738585072014e-308) throw new NotSupportedException($"{where}: float contract default must not be subnormal");
                    if (d == 0.0) d = 0.0; // normalise -0.0
                    byte[] le = new byte[8];
                    System.Buffers.Binary.BinaryPrimitives.WriteDoubleLittleEndian(le, d);
                    return le;
                }
                default:
                    throw new NotSupportedException($"{where}: contract defaults for wire type '{primitive}' are reserved in v1 (§11.3.2.3); supported: string, bool, int, long, double, empty containers");
            }
        }

        private static byte[] UVarint(ulong n)
        {
            var o = new List<byte>(10);
            while (true)
            {
                byte b = (byte)(n & 0x7F);
                n >>= 7;
                if (n != 0) o.Add((byte)(b | 0x80));
                else { o.Add(b); return [.. o]; }
            }
        }

        private (string Kind, string Primitive, string Ref) Leaf(Type t)
        {
            if (Nullable.GetUnderlyingType(t) is { } inner) t = inner;
            string? prim = Primitive(t);
            if (prim != null) return ("primitive", prim, "");
            if (t == typeof(object)) return ("any", "", "");
            if (t.GetCustomAttribute<WireTypeAttribute>() != null) return ("ref", "", Hash(t));
            throw new NotSupportedException($"type {t.FullName} has no contract mapping (add [WireType] or use a supported primitive)");
        }

        private static string? Primitive(Type t) =>
            t == typeof(string) ? "string" :
            t == typeof(bool) ? "bool" :
            t == typeof(sbyte) ? "int8" :
            t == typeof(short) ? "int16" :
            t == typeof(int) ? "varint32" :
            t == typeof(long) ? "varint64" :
            t == typeof(byte) ? "uint8" :
            t == typeof(ushort) ? "uint16" :
            t == typeof(uint) ? "var_uint32" :
            t == typeof(ulong) ? "var_uint64" :
            t == typeof(float) ? "float32" :
            t == typeof(double) ? "float64" :
            t == typeof(byte[]) ? "binary" :
            null;

        private static Type[]? Generic(Type t, Type openInterface)
        {
            if (t.IsGenericType && t.GetGenericTypeDefinition() == openInterface) return t.GetGenericArguments();
            foreach (var i in t.GetInterfaces())
                if (i.IsGenericType && i.GetGenericTypeDefinition() == openInterface) return i.GetGenericArguments();
            return null;
        }
    }

    private static unsafe string Blake3Hex(byte[] bytes)
    {
        byte* outBuf = stackalloc byte[64];
        UIntPtr len = 64;
        fixed (byte* p = bytes)
            Native.Check(Native.aster_blake3_hex(p, (UIntPtr)bytes.Length, outBuf, &len), "aster_blake3_hex");
        return Encoding.ASCII.GetString(outBuf, (int)len);
    }
}

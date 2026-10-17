/**
 * Field names (`ffi_spec/Aster-FieldNaming.md`). A field's contract name is
 * the TypeScript property name, verbatim, or its `@WireName('...')` when the
 * contract is defined in another language. Fory JS matches fields by its
 * snake_case form of the property name, so contract definition (`aster-gen`)
 * refuses a name that is not plain, a property whose Fory form differs from
 * the snake_case form of its name, and two fields with one snake_case form.
 */

const isUpper = (c: string): boolean => c >= 'A' && c <= 'Z';
const isLowerOrDigit = (c: string): boolean => (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9');
const isNonAsciiUpper = (c: string): boolean => c.codePointAt(0)! > 127 && /\p{Lu}/u.test(c);

/**
 * The reference snake_case form: Rust fory-core's `to_snake_case`, which Fory
 * JS's `TypeMeta.toSnakeCase` also implements (the conformance vectors pin
 * both).
 */
export function referenceSnakeCase(name: string): string {
  const cs = Array.from(name);
  let out = '';
  for (let i = 0; i < cs.length; i++) {
    const c = cs[i]!;
    if (isUpper(c)) {
      if (i > 0) {
        const prevUpper = isUpper(cs[i - 1]!);
        const nextUpperOrEnd = i + 1 >= cs.length || isUpper(cs[i + 1]!);
        if (!prevUpper || !nextUpperOrEnd) out += '_';
      }
      out += c.toLowerCase();
    } else {
      out += c;
    }
  }
  return out;
}

/**
 * Why `name` is not plain, or `undefined` when it is: every ASCII capital
 * starts the name or follows an ASCII lowercase letter or a digit, no
 * non-ASCII letter is uppercase, and the name does not end in `_`.
 */
export function notPlain(name: string): string | undefined {
  const cs = Array.from(name);
  for (let i = 0; i < cs.length; i++) {
    const c = cs[i]!;
    if (isUpper(c) && i > 0 && !isLowerOrDigit(cs[i - 1]!)) {
      return 'an uppercase letter follows another uppercase letter, `_` or a non-ASCII character';
    }
    if (isNonAsciiUpper(c)) return 'it has an uppercase letter outside ASCII';
  }
  return name.endsWith('_') ? 'it ends in `_`' : undefined;
}

/** A plain spelling of `name` (`userID` -> `userId`, `HTTPServer` -> `HttpServer`). */
export function plainSuggestion(name: string): string {
  const rs = Array.from(name);
  let i = 0;
  while (i < rs.length) {
    if (!isUpper(rs[i]!)) {
      if (isNonAsciiUpper(rs[i]!)) rs[i] = rs[i]!.toLowerCase();
      i++;
      continue;
    }
    let end = i;
    while (end < rs.length && isUpper(rs[end]!)) end++;
    const nextLower = end < rs.length && rs[end]! >= 'a' && rs[end]! <= 'z';
    const keepLast = nextLower && end - i >= 3;
    const firstOk = i === 0 || isLowerOrDigit(rs[i - 1]!);
    for (let k = i; k < end; k++) {
      if (!((k === i && firstOk) || (k === end - 1 && keepLast))) rs[k] = rs[k]!.toLowerCase();
    }
    i = end;
  }
  return rs.join('').replace(/_+$/, '');
}

/** One field: Fory's snake_case form of the member, and the contract name. */
export interface FieldNameEntry {
  memberSnake: string;
  name: string;
}

/** Check one type's fields; throws an `Error` naming the first offending field. */
export function checkFieldNames(fields: readonly FieldNameEntry[]): void {
  for (const { memberSnake, name } of fields) {
    const why = notPlain(name);
    if (why) {
      throw new Error(
        `"${name}" is not a plain field name: ${why}, so the Fory implementations disagree on its ` +
        `snake_case form. Use a plain spelling such as "${plainSuggestion(name)}" (ffi_spec/Aster-FieldNaming.md §5)`,
      );
    }
    const snake = referenceSnakeCase(name);
    if (snake !== memberSnake) {
      throw new Error(
        `the field name "${name}" does not match its member: Fory writes the member as "${memberSnake}" ` +
        `but "${name}" is "${snake}". Name the member so its snake_case form is "${snake}", with ` +
        `@WireName('${name}') (ffi_spec/Aster-FieldNaming.md §4)`,
      );
    }
  }
  for (let i = 0; i < fields.length; i++) {
    for (let j = 0; j < i; j++) {
      const a = fields[j]!.name;
      const b = fields[i]!.name;
      if (referenceSnakeCase(a) === referenceSnakeCase(b)) {
        throw new Error(
          `the fields "${a}" and "${b}" collide: Fory writes both as "${referenceSnakeCase(b)}" and ` +
          `could not tell them apart (ffi_spec/Aster-FieldNaming.md §4)`,
        );
      }
    }
  }
}

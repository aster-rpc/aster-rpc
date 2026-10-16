//! Field names (`ffi_spec/Aster-FieldNaming.md`).
//!
//! A field's contract name is the declared name, verbatim, or an explicit
//! `#[aster(name = "...")]` override. Fory matches fields by a snake_case form
//! of the member name, so a contract is refused when it is defined with
//!
//! - a name that is not *plain* (§5): one the Fory implementations'
//!   snake_case conversions do not all agree on;
//! - an override whose snake_case form differs from the member's (§4.2);
//! - two fields with the same snake_case form (§4.1).

/// Fory's snake_case form of a field name, as Rust `fory-core` 1.7.6
/// (`util::to_snake_case`) and Fory JavaScript write it. This is the reference
/// form every binding compares against; for a plain name every Fory
/// implementation agrees with it (`conformance/vectors/contract-types.json`,
/// `field_names.snake_case`).
pub(crate) fn fory_snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                let prev_upper = chars[i - 1].is_ascii_uppercase();
                let next_upper_or_end = chars.get(i + 1).is_none_or(|n| n.is_ascii_uppercase());
                if !prev_upper || !next_upper_or_end {
                    out.push('_');
                }
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Why `name` is not plain, or `None` when it is. Plain (§5): every ASCII
/// uppercase letter starts the name or follows an ASCII lowercase letter or a
/// digit, no letter outside ASCII is uppercase, and the name does not end in
/// `_`.
pub(crate) fn not_plain(name: &str) -> Option<&'static str> {
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase()
            && i > 0
            && !(chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit())
        {
            return Some("an uppercase letter follows another uppercase letter, `_` or a non-ASCII character");
        }
        if !c.is_ascii() && c.is_uppercase() {
            return Some("it has an uppercase letter outside ASCII");
        }
    }
    if name.ends_with('_') {
        return Some("it ends in `_`");
    }
    None
}

/// A plain spelling to suggest for `name`: an acronym written as a word
/// (`userID` -> `userId`, `HTTPServer` -> `HttpServer`, `IPv6` -> `Ipv6`), an
/// uppercase letter after `_` or a non-ASCII character lowered, non-ASCII
/// uppercase letters lowered, and a trailing `_` dropped.
pub(crate) fn plain_suggestion(name: &str) -> String {
    let mut chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_uppercase() {
            if !chars[i].is_ascii() && chars[i].is_uppercase() {
                let lower: Vec<char> = chars[i].to_lowercase().collect();
                chars.splice(i..=i, lower);
            }
            i += 1;
            continue;
        }
        // A run of ASCII uppercase letters [i, end).
        let mut end = i;
        while end < chars.len() && chars[end].is_ascii_uppercase() {
            end += 1;
        }
        let next_lower = chars.get(end).is_some_and(|c| c.is_ascii_lowercase());
        // The run's last letter starts the next word in `HTTPServer`.
        let keep_last = next_lower && end - i >= 3;
        let first_ok = i == 0 || chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit();
        for (k, c) in chars.iter_mut().enumerate().take(end).skip(i) {
            let keep = (k == i && first_ok) || (k == end - 1 && keep_last);
            if !keep {
                *c = c.to_ascii_lowercase();
            }
        }
        i = end;
    }
    let out: String = chars.into_iter().collect();
    out.trim_end_matches('_').to_string()
}

/// Check one type's fields, each as (the member's Fory snake_case form, its
/// contract name). `Err((index, message))` names the first offending field.
pub(crate) fn check(fields: &[(String, String)]) -> Result<(), (usize, String)> {
    for (i, (member_snake, name)) in fields.iter().enumerate() {
        if let Some(why) = not_plain(name) {
            return Err((
                i,
                format!(
                    "`{name}` is not a plain field name: {why}, so the Fory implementations \
                     disagree on its snake_case form. Use a plain spelling such as `{}` \
                     (ffi_spec/Aster-FieldNaming.md §5)",
                    plain_suggestion(name)
                ),
            ));
        }
        let snake = fory_snake_case(name);
        if &snake != member_snake {
            return Err((
                i,
                format!(
                    "the field name `{name}` does not match its member: Fory writes the member \
                     as `{member_snake}` but `{name}` is `{snake}`. Name the member so its \
                     snake_case form is `{snake}` (ffi_spec/Aster-FieldNaming.md §4)"
                ),
            ));
        }
    }
    for (i, (_, b)) in fields.iter().enumerate() {
        for (_, a) in &fields[..i] {
            if fory_snake_case(a) == fory_snake_case(b) {
                return Err((
                    i,
                    format!(
                        "the fields `{a}` and `{b}` collide: Fory writes both as `{}` and \
                         could not tell them apart (ffi_spec/Aster-FieldNaming.md §4)",
                        fory_snake_case(b)
                    ),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn field_names() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../conformance/vectors/contract-types.json"
        );
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        doc["vector"]["field_names"].clone()
    }

    #[test]
    fn snake_case_and_plainness_match_the_vector() {
        let v = field_names();
        for (name, row) in v["snake_case"].as_object().unwrap() {
            assert_eq!(fory_snake_case(name), row["rust"], "{name}");
            assert_eq!(not_plain(name).is_none(), row["plain"], "{name}");
            if row["plain"] == false {
                let s = plain_suggestion(name);
                assert!(not_plain(&s).is_none(), "{name} -> {s}");
            }
        }
    }

    #[test]
    fn refusals_and_acceptances_match_the_vector() {
        let v = field_names();
        let pairs = |c: &Value| -> Vec<(String, String)> {
            c["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    (
                        f["member_snake"].as_str().unwrap().to_string(),
                        f["name"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        };
        for c in v["refused"].as_array().unwrap() {
            let (_, msg) = check(&pairs(c)).expect_err(&c.to_string());
            assert!(msg.contains(c["error"].as_str().unwrap()), "{c}: {msg}");
        }
        for c in v["accepted"].as_array().unwrap() {
            check(&pairs(c)).unwrap_or_else(|e| panic!("{c}: {e:?}"));
        }
    }

    #[test]
    fn suggestions() {
        assert_eq!(plain_suggestion("userID"), "userId");
        assert_eq!(plain_suggestion("HTTPServer"), "HttpServer");
        assert_eq!(plain_suggestion("ID"), "Id");
        assert_eq!(plain_suggestion("type_"), "type");
        assert_eq!(plain_suggestion("a_B"), "a_b");
        assert_eq!(plain_suggestion("IPv6Addr"), "Ipv6Addr");
        assert_eq!(plain_suggestion("getHTTPResponse"), "getHttpResponse");
    }
}

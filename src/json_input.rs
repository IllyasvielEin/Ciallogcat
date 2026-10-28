//! Guard a serde_json arbitrary-precision implementation detail at input boundaries.
//! Its private map representation cannot safely preserve a literal object key.

/// Parse bounded JSON supplied by a caller without accepting the internal
/// arbitrary-precision object spelling. The caller owns its input byte limit.
pub fn from_slice<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    reject_ambiguous_number_key(bytes).map_err(<serde_json::Error as serde::de::Error>::custom)?;
    serde_json::from_slice(bytes)
}

pub(crate) fn reject_ambiguous_number_key(bytes: &[u8]) -> Result<(), String> {
    const KEY: &str = "$serde_json::private::Number";
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'"' {
            at += 1;
            continue;
        }
        let start = at;
        at += 1;
        while at < bytes.len() && bytes[at] != b'"' {
            at += if bytes[at] == b'\\' { 2 } else { 1 };
        }
        if at >= bytes.len() {
            break;
        }
        let key = &bytes[start + 1..at];
        at += 1;
        if bytes[at..].iter().find(|b| !b.is_ascii_whitespace()) != Some(&b':') {
            continue;
        }
        let reserved = key == KEY.as_bytes()
            || (key.contains(&b'\\')
                && serde_json::from_slice::<String>(&bytes[start..at])
                    .map_err(|error| error.to_string())?
                    == KEY);
        if reserved {
            return Err("unsupported JSON object key $serde_json::private::Number (ambiguous with arbitrary-precision numbers)".into());
        }
    }
    Ok(())
}

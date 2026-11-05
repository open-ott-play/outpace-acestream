//! Minimal bencode (BitTorrent encoding). Byte-string keys; canonical encode.
use crate::{Result, WireError};
use std::collections::BTreeMap;

/// Maximum container nesting accepted when parsing untrusted bencode. Peer extended
/// handshakes and DHT replies are shallow in practice; this bounds recursion so a
/// small deeply nested value cannot exhaust the stack.
const MAX_DEPTH: usize = 32;

/// Bound allocations for shallow messages too: a compact list of empty values can
/// otherwise expand a valid peer frame into tens of MiB. Dictionary keys count
/// toward the same budget, including keys that overwrite earlier entries.
const MAX_VALUES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bencode {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Bencode>),
    Dict(BTreeMap<Vec<u8>, Bencode>),
}

impl Bencode {
    /// Parse exactly one bencode value from the whole buffer; trailing bytes = error.
    pub fn parse(buf: &[u8]) -> Result<Bencode> {
        let (v, n) = Self::parse_prefix(buf)?;
        if n != buf.len() {
            return Err(WireError::Invalid("trailing bytes"));
        }
        Ok(v)
    }

    /// Parse one value from the front; return (value, bytes_consumed).
    pub fn parse_prefix(buf: &[u8]) -> Result<(Bencode, usize)> {
        let mut remaining = MAX_VALUES;
        parse_value(buf, 0, 0, &mut remaining)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Bencode::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(i.to_string().as_bytes());
                out.push(b'e');
            }
            Bencode::Bytes(b) => {
                out.extend_from_slice(b.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(b);
            }
            Bencode::List(l) => {
                out.push(b'l');
                for e in l {
                    e.encode_into(out);
                }
                out.push(b'e');
            }
            Bencode::Dict(d) => {
                out.push(b'd');
                for (k, v) in d {
                    Bencode::Bytes(k.clone()).encode_into(out);
                    v.encode_into(out);
                }
                out.push(b'e');
            }
        }
    }

    /// Convenience: borrow a dict entry.
    pub fn get<'a>(&'a self, key: &[u8]) -> Option<&'a Bencode> {
        match self {
            Bencode::Dict(d) => d.get(key),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        if let Bencode::Int(i) = self {
            Some(*i)
        } else {
            None
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        if let Bencode::Bytes(b) = self {
            Some(b)
        } else {
            None
        }
    }
}

/// `depth` is the number of container (list/dict) ancestors already open. It bounds
/// recursion so untrusted, deeply nested bencode cannot exhaust the stack.
fn parse_value(
    buf: &[u8],
    pos: usize,
    depth: usize,
    remaining: &mut usize,
) -> Result<(Bencode, usize)> {
    consume_value(remaining)?;
    match buf.get(pos).ok_or(WireError::Truncated)? {
        b'i' => parse_int(buf, pos),
        b'l' | b'd' if depth >= MAX_DEPTH => Err(WireError::Invalid("bencode nesting too deep")),
        b'l' => parse_list(buf, pos, depth, remaining),
        b'd' => parse_dict(buf, pos, depth, remaining),
        b'0'..=b'9' => parse_bytes(buf, pos),
        _ => Err(WireError::Invalid("unexpected bencode token")),
    }
}

fn consume_value(remaining: &mut usize) -> Result<()> {
    if *remaining == 0 {
        return Err(WireError::Invalid("too many bencode values"));
    }
    *remaining -= 1;
    Ok(())
}

fn parse_int(buf: &[u8], pos: usize) -> Result<(Bencode, usize)> {
    // buf[pos] == 'i'
    let end = buf[pos + 1..]
        .iter()
        .position(|&b| b == b'e')
        .ok_or(WireError::Truncated)?
        + pos
        + 1;
    let s = std::str::from_utf8(&buf[pos + 1..end]).map_err(|_| WireError::Invalid("int utf8"))?;
    let i = s
        .parse::<i64>()
        .map_err(|_| WireError::Invalid("int parse"))?;
    Ok((Bencode::Int(i), end + 1))
}

fn parse_bytes(buf: &[u8], pos: usize) -> Result<(Bencode, usize)> {
    let colon = buf[pos..]
        .iter()
        .position(|&b| b == b':')
        .ok_or(WireError::Truncated)?
        + pos;
    let len: usize = std::str::from_utf8(&buf[pos..colon])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or(WireError::Invalid("bad length"))?;
    let start = colon + 1;
    let end = start
        .checked_add(len)
        .ok_or(WireError::Invalid("len overflow"))?;
    if end > buf.len() {
        return Err(WireError::Truncated);
    }
    Ok((Bencode::Bytes(buf[start..end].to_vec()), end))
}

fn parse_list(
    buf: &[u8],
    pos: usize,
    depth: usize,
    remaining: &mut usize,
) -> Result<(Bencode, usize)> {
    let mut i = pos + 1;
    let mut items = Vec::new();
    loop {
        match buf.get(i).ok_or(WireError::Truncated)? {
            b'e' => return Ok((Bencode::List(items), i + 1)),
            _ => {
                let (v, n) = parse_value(buf, i, depth + 1, remaining)?;
                items.push(v);
                i = n;
            }
        }
    }
}

fn parse_dict(
    buf: &[u8],
    pos: usize,
    depth: usize,
    remaining: &mut usize,
) -> Result<(Bencode, usize)> {
    let mut i = pos + 1;
    let mut map = BTreeMap::new();
    loop {
        match buf.get(i).ok_or(WireError::Truncated)? {
            b'e' => return Ok((Bencode::Dict(map), i + 1)),
            _ => {
                consume_value(remaining)?;
                let (k, n) = parse_bytes(buf, i)?;
                let key = if let Bencode::Bytes(b) = k {
                    b
                } else {
                    unreachable!()
                };
                let (v, n2) = parse_value(buf, n, depth + 1, remaining)?;
                map.insert(key, v);
                i = n2;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple() {
        // d3:bar4:spam3:fooi42ee  -> {bar: "spam", foo: 42}
        let raw = b"d3:bar4:spam3:fooi42ee";
        let v = Bencode::parse(raw).unwrap();
        let mut d = std::collections::BTreeMap::new();
        d.insert(b"bar".to_vec(), Bencode::Bytes(b"spam".to_vec()));
        d.insert(b"foo".to_vec(), Bencode::Int(42));
        assert_eq!(v, Bencode::Dict(d));
        assert_eq!(v.encode(), raw); // canonical re-encode (keys sorted)
    }

    #[test]
    fn parses_list_and_negative_int() {
        let v = Bencode::parse(b"li-3e1:ae").unwrap();
        assert_eq!(
            v,
            Bencode::List(vec![Bencode::Int(-3), Bencode::Bytes(b"a".to_vec())])
        );
    }

    #[test]
    fn rejects_trailing_and_truncated() {
        assert!(Bencode::parse(b"i42").is_err()); // truncated
        assert!(Bencode::parse(b"i42eX").is_err()); // trailing byte
        assert!(Bencode::parse(b"3:ab").is_err()); // short string
    }

    /// Build a value nested `depth` levels deep: `depth` opening tokens, an inner
    /// int, then `depth` closing tokens. `container` is b'l' (list) or b'd' (dict).
    fn nested(container: u8, depth: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        for _ in 0..depth {
            if container == b'd' {
                buf.extend_from_slice(b"d1:k"); // open dict, then a key for the nested value
            } else {
                buf.push(b'l'); // open list
            }
        }
        buf.extend_from_slice(b"i0e");
        buf.resize(buf.len() + depth, b'e');
        buf
    }

    #[test]
    fn accepts_nesting_at_the_limit() {
        assert!(Bencode::parse(&nested(b'l', MAX_DEPTH)).is_ok());
        assert!(Bencode::parse(&nested(b'd', MAX_DEPTH)).is_ok());
    }

    #[test]
    fn rejects_nesting_beyond_the_limit() {
        assert!(Bencode::parse(&nested(b'l', MAX_DEPTH + 1)).is_err());
        assert!(Bencode::parse(&nested(b'd', MAX_DEPTH + 1)).is_err());
    }

    #[test]
    fn rejects_pathologically_deep_nesting_without_overflow() {
        // A small input can still describe very deep recursion; this must return a
        // clean error rather than exhausting the stack.
        assert!(Bencode::parse(&nested(b'l', 100_000)).is_err());
    }

    fn flat_list(items: usize) -> Vec<u8> {
        let mut bytes = vec![b'l'];
        for _ in 0..items {
            bytes.extend_from_slice(b"le");
        }
        bytes.push(b'e');
        bytes
    }

    #[test]
    fn bounds_shallow_list_values_including_the_root() {
        let at_limit = flat_list(MAX_VALUES - 1);
        let Bencode::List(items) = Bencode::parse(&at_limit).unwrap() else {
            panic!("expected list");
        };
        assert_eq!(items.len(), MAX_VALUES - 1);
        assert_eq!(
            Bencode::parse(&flat_list(MAX_VALUES)),
            Err(WireError::Invalid("too many bencode values")),
        );
    }

    fn flat_dictionary(duplicate_keys: bool, extra_value: bool) -> Vec<u8> {
        let mut bytes = vec![b'd'];
        // Root + key/value pairs consume MAX_VALUES - 3 values. The final
        // key, list and empty nested list use the remaining three exactly.
        for index in 0..(MAX_VALUES - 4) / 2 {
            let key = if duplicate_keys {
                "k".to_string()
            } else {
                format!("k{index}")
            };
            bytes.extend_from_slice(format!("{}:{key}i1e", key.len()).as_bytes());
        }
        bytes.extend_from_slice(b"1:zlle");
        if extra_value {
            bytes.extend_from_slice(b"le");
        }
        bytes.extend_from_slice(b"ee");
        bytes
    }

    #[test]
    fn bounds_dictionary_keys_and_values_even_when_keys_are_overwritten() {
        for duplicate_keys in [false, true] {
            assert!(Bencode::parse(&flat_dictionary(duplicate_keys, false)).is_ok());
            assert_eq!(
                Bencode::parse(&flat_dictionary(duplicate_keys, true)),
                Err(WireError::Invalid("too many bencode values")),
            );
        }
    }

    #[test]
    fn prefix_budget_does_not_count_opaque_trailing_bytes() {
        let mut bytes = flat_list(MAX_VALUES - 1);
        let consumed = bytes.len();
        bytes.extend_from_slice(&flat_list(MAX_VALUES));
        let (value, actual) = Bencode::parse_prefix(&bytes).unwrap();
        assert_eq!(actual, consumed);
        assert!(matches!(value, Bencode::List(_)));
        assert_eq!(
            Bencode::parse(&bytes),
            Err(WireError::Invalid("trailing bytes"))
        );
    }

    #[test]
    fn preserves_permissive_integer_and_duplicate_key_handling() {
        let value = Bencode::parse(b"d1:ki01e1:ki+2ee").unwrap();
        assert_eq!(value.get(b"k").and_then(Bencode::as_int), Some(2));
    }
}

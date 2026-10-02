use crate::wire::SecretBuf;

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

pub(crate) const fn encoded_len(bytes: usize) -> usize {
    (bytes * 8).div_ceil(5)
}

pub(crate) fn encode_into(data: &[u8], out: &mut String) {
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &byte in data {
        acc = (acc << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(symbol(acc >> bits));
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(symbol(acc << (5 - bits)));
    }
}

fn symbol(value: u32) -> char {
    char::from(ALPHABET[(value & 31) as usize])
}

fn value(symbol: u8) -> Option<u32> {
    match symbol {
        b'a'..=b'z' => Some(u32::from(symbol - b'a')),
        b'A'..=b'Z' => Some(u32::from(symbol - b'A')),
        b'2'..=b'7' => Some(u32::from(symbol - b'2') + 26),
        _ => None,
    }
}

pub(crate) fn decode(text: &[u8]) -> Option<SecretBuf> {
    let mut out = SecretBuf::with_capacity(text.len() * 5 / 8);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &c in text {
        acc = (acc << 5) | value(c)?;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8)?;
            acc &= (1 << bits) - 1;
        }
    }
    // Five or more bits left over means one symbol too many for any byte count. Non-zero
    // leftover bits would give a second spelling of the same bytes, which a checksum over
    // the bytes cannot catch.
    if bits >= 5 || acc != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn encode(data: &[u8]) -> String {
        let mut out = String::new();
        encode_into(data, &mut out);
        out
    }

    #[test]
    fn rfc4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ];
        for (plain, coded) in vectors {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(encoded_len(plain.len()), coded.len());
            assert_eq!(decode(coded.as_bytes()).as_deref(), Some(plain.as_bytes()));
            let upper = coded.to_ascii_uppercase();
            assert_eq!(decode(upper.as_bytes()).as_deref(), Some(plain.as_bytes()));
        }
    }

    #[test]
    fn impossible_lengths_are_rejected() {
        for text in ["m", "mzx", "mzxw6y", "mzxw6ytbo"] {
            assert!(decode(text.as_bytes()).is_none(), "{text}");
        }
    }

    // A zero symbol after text that ends on 0, 1 or 2 padding bits adds only zero bits and no
    // byte, so the leftover-bits check cannot see it. Only the length rule can.
    #[test]
    fn one_symbol_too_many_is_rejected() {
        for text in ["a", "mya", "mzxw6a", "mzxw6ytba", "mzxw6ytboia"] {
            assert!(decode(text.as_bytes()).is_none(), "{text}");
        }
    }

    #[test]
    fn non_zero_trailing_bits_are_rejected() {
        for text in ["mz", "mzxr", "mzxw6yr"] {
            assert!(decode(text.as_bytes()).is_none(), "{text}");
        }
    }

    #[test]
    fn characters_outside_the_alphabet_are_rejected() {
        for text in ["m0", "m1", "m8", "m9", "my==", "m-", "m y", "m\u{e9}"] {
            assert!(decode(text.as_bytes()).is_none(), "{text}");
        }
    }

    proptest! {
        #[test]
        fn round_trip(data in proptest::collection::vec(any::<u8>(), 0..200)) {
            let text = encode(&data);
            prop_assert_eq!(text.len(), encoded_len(data.len()));
            let decoded = decode(text.as_bytes());
            prop_assert_eq!(decoded.as_deref(), Some(&data[..]));
        }
    }
}

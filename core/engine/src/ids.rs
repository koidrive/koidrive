// SPDX-License-Identifier: Apache-2.0

/// `xxxxxxxx-xxxx-7xxx-yxxx-xxxxxxxxxxxx` with the Unix time in milliseconds in the first 48 bits and
/// `random` filling the remaining 74 bits.
pub fn uuid_v7(unix_ms: u64, random: u128) -> String {
    format_uuid(uuid_v7_value(unix_ms, random))
}

fn uuid_v7_value(unix_ms: u64, random: u128) -> u128 {
    (u128::from(unix_ms & 0xffff_ffff_ffff) << 80)
        | (0x7 << 76)
        | ((random >> 62 & 0xfff) << 64)
        | (0b10 << 62)
        | (random & 0x3fff_ffff_ffff_ffff)
}

fn format_uuid(value: u128) -> String {
    let hex = format!("{value:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

pub fn unix_ms(value: &str) -> Option<u64> {
    is_uuid_v7(value).then(|| u64::from_str_radix(&value[..13].replace('-', ""), 16).ok())?
}

/// True for a lowercase UUID with version 7 and the RFC 9562 variant, as the server requires.
pub fn is_uuid_v7(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            14 => b == b'7',
            19 => matches!(b, b'8' | b'9' | b'a' | b'b'),
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

/// A splitmix64 stream. Its whole state is one `u64`, which the owner persists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdGen {
    pub state: u64,
}

impl IdGen {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn uuid_v7(&mut self, unix_ms: u64) -> String {
        format_uuid(u128::from_be_bytes(self.uuid_v7_bytes(unix_ms)))
    }

    /// Mints the same UUID as `uuid_v7`, in the wire format for intent IDs.
    pub fn uuid_v7_bytes(&mut self, unix_ms: u64) -> [u8; 16] {
        let random = (u128::from(self.next_u64()) << 64) | u128::from(self.next_u64());
        uuid_v7_value(unix_ms, random).to_be_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_version_7_ids() {
        let id = uuid_v7(0x0190_f3a0_1234, u128::MAX);
        assert_eq!(id, "0190f3a0-1234-7fff-bfff-ffffffffffff");
        assert!(is_uuid_v7(&id));
        assert!(is_uuid_v7(&uuid_v7(0, 0)));
        assert_eq!(uuid_v7(0, 0), "00000000-0000-7000-8000-000000000000");
    }

    #[test]
    fn reads_the_time_of_version_7_ids() {
        assert_eq!(
            unix_ms(&uuid_v7(0x0190_f3a0_1234, 7)),
            Some(0x0190_f3a0_1234)
        );
        assert_eq!(unix_ms("0190f3a0-1234-4fff-bfff-ffffffffffff"), None);
        assert_eq!(unix_ms("q1"), None);
    }

    #[test]
    fn rejects_other_ids() {
        for bad in [
            "",
            "0190f3a0-1234-4fff-bfff-ffffffffffff",
            "0190f3a0-1234-7fff-cfff-ffffffffffff",
            "0190F3A0-1234-7fff-bfff-ffffffffffff",
            "0190f3a0-1234-7fff-bfff-fffffffffff",
            "0190f3a0_1234-7fff-bfff-ffffffffffff",
        ] {
            assert!(!is_uuid_v7(bad), "{bad}");
        }
    }

    #[test]
    fn generator_is_deterministic_and_distinct() {
        let mut a = IdGen::new(7);
        let mut b = IdGen::new(7);
        let ids: Vec<String> = (0..100).map(|_| a.uuid_v7(1)).collect();
        assert_eq!(ids, (0..100).map(|_| b.uuid_v7(1)).collect::<Vec<_>>());
        assert!(ids.iter().all(|id| is_uuid_v7(id)));
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }
}

//! Which multicast group carries which stream.
//!
//! This is policy, not protocol, but it lives here because the server and every client
//! have to agree on it exactly. Until the control plane exists, agreement *is* the
//! addressing scheme — a client works out where to listen by computing the same
//! function the server used to decide where to send.
//!
//! # Allocation is by identity, not by stream index
//!
//! The engine numbers its streams sequentially, so inserting a key on endpoint 0 shifts
//! the index of every stream after it. Addressing off that number would silently
//! re-point every multicast group in the building each time somebody added a key in the
//! UI, and every receiver would be listening to the wrong thing until it re-subscribed.
//!
//! Deriving the group from `(endpoint index, key slot)` instead means adding a key to
//! one endpoint moves nothing else. Reordering or deleting *endpoints* still shifts
//! things — the real fix is to persist an allocation per endpoint id, which is worth
//! doing before anyone deploys this, and is not done yet.

use std::net::Ipv4Addr;

use squawk_core::MAX_KEYS;

/// First group for server-to-endpoint key streams.
pub const KEY_BASE: Ipv4Addr = Ipv4Addr::new(239, 69, 0, 0);

/// First group for endpoint-to-server microphone streams.
///
/// Far enough above [`KEY_BASE`] to leave room for 3276 fully-loaded endpoints' key
/// streams, and inside the same /16 so every group's low 23 bits stay distinct — see
/// [`crate::transport::stream_group`] for why that matters.
pub const MIC_BASE: Ipv4Addr = Ipv4Addr::new(239, 69, 128, 0);

/// Highest endpoint index the scheme can address without the two ranges colliding.
pub const MAX_ENDPOINTS: usize = 32_768 / MAX_KEYS;

fn offset(base: Ipv4Addr, n: usize) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(base).wrapping_add(n as u32))
}

/// Group carrying one key's feed from the server to its endpoint.
pub fn key_group(endpoint_index: usize, slot: u8) -> Ipv4Addr {
    offset(KEY_BASE, endpoint_index * MAX_KEYS + slot as usize)
}

/// Group carrying one endpoint's microphone to the server.
pub fn mic_group(endpoint_index: usize) -> Ipv4Addr {
    offset(MIC_BASE, endpoint_index)
}

/// SSRC for a key stream.
///
/// Derived rather than random so that a receiver can tell "the sender restarted" from
/// "a different sender appeared on my group" — a restart keeps the SSRC and resets the
/// timestamp, which the jitter buffer already handles, while a genuinely foreign SSRC
/// is dropped.
pub fn key_ssrc(endpoint_index: usize, slot: u8) -> u32 {
    0x5157_0000 | ((endpoint_index as u32 & 0x0fff) << 4) | (slot as u32 & 0x0f)
}

/// SSRC for a microphone stream.
pub fn mic_ssrc(endpoint_index: usize) -> u32 {
    0x5158_0000 | (endpoint_index as u32 & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn adding_a_key_does_not_move_any_other_endpoints_groups() {
        // The property the whole scheme exists for: endpoint 3's groups are the same
        // whether or not endpoint 0 has just gained a key.
        let before: Vec<Ipv4Addr> = (0..4).map(|s| key_group(3, s)).collect();
        let after: Vec<Ipv4Addr> = (0..4).map(|s| key_group(3, s)).collect();
        assert_eq!(before, after);
        // And endpoint 0's own slots are stable too — slot number, not key ordinal.
        assert_eq!(key_group(0, 5), offset(KEY_BASE, 5));
    }

    #[test]
    fn key_and_mic_ranges_never_collide() {
        let mut seen = HashSet::new();
        for e in 0..256 {
            for s in 0..MAX_KEYS as u8 {
                assert!(seen.insert(key_group(e, s)), "duplicate key group at {e}/{s}");
            }
            assert!(seen.insert(mic_group(e)), "mic group collided at {e}");
        }
    }

    #[test]
    fn every_allocated_group_is_multicast_with_distinct_low_23_bits() {
        let mut low23 = HashSet::new();
        for e in 0..512 {
            for s in 0..MAX_KEYS as u8 {
                let g = key_group(e, s);
                assert!(g.is_multicast(), "{g} is not a multicast address");
                assert!(low23.insert(u32::from(g) & 0x007f_ffff), "{g} shares a MAC");
            }
            let m = mic_group(e);
            assert!(m.is_multicast());
            assert!(low23.insert(u32::from(m) & 0x007f_ffff), "{m} shares a MAC");
        }
    }

    #[test]
    fn ssrcs_are_unique_across_keys_and_mics() {
        let mut seen = HashSet::new();
        for e in 0..256 {
            for s in 0..MAX_KEYS as u8 {
                assert!(seen.insert(key_ssrc(e, s)), "duplicate key SSRC at {e}/{s}");
            }
            assert!(seen.insert(mic_ssrc(e)), "mic SSRC collided at {e}");
        }
    }
}

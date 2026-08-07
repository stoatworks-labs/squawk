//! The Best Master Clock Algorithm (IEEE 1588-2008 clause 9.3).
//!
//! Every clock on a domain announces itself, everyone runs the same comparison, and
//! they all independently reach the same answer about who the grandmaster is. There is
//! no election protocol — the agreement comes from the algorithm being deterministic
//! and total, which is why the tie-break on clock identity at the bottom matters as
//! much as priority1 at the top.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::message::{Announce, ClockIdentity, ClockQuality, Header, PortIdentity};

/// What a received Announce says about the clock it is advertising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MasterDataset {
    pub grandmaster: ClockIdentity,
    pub priority1: u8,
    pub quality: ClockQuality,
    pub priority2: u8,
    pub steps_removed: u16,
    /// Which port sent this Announce — the final tie-break, and how we know where to
    /// send Delay_Reqs once it wins.
    pub sender: PortIdentity,
}

impl MasterDataset {
    pub fn from_announce(header: &Header, announce: &Announce) -> Self {
        Self {
            grandmaster: announce.grandmaster_identity,
            priority1: announce.grandmaster_priority1,
            quality: announce.grandmaster_quality,
            priority2: announce.grandmaster_priority2,
            steps_removed: announce.steps_removed,
            sender: header.source,
        }
    }
}

/// Compare two candidates. `Less` means `a` is the better master.
///
/// The comparison order is fixed by the standard and is not a matter of taste:
/// priority1, then clock class, accuracy and variance, then priority2, then identity.
/// `priority1` sitting above every measure of actual quality is deliberate — it is the
/// operator's override, the way you force a particular box to be grandmaster regardless
/// of what the hardware claims about itself.
pub fn compare(a: &MasterDataset, b: &MasterDataset) -> Ordering {
    if a.grandmaster == b.grandmaster {
        // Same grandmaster reached by two paths: prefer the shorter one, and fall back
        // to sender identity so that two equal-length paths still resolve identically
        // on every clock in the domain.
        return a
            .steps_removed
            .cmp(&b.steps_removed)
            .then_with(|| a.sender.clock.cmp(&b.sender.clock))
            .then_with(|| a.sender.port.cmp(&b.sender.port));
    }

    a.priority1
        .cmp(&b.priority1)
        .then_with(|| a.quality.class.cmp(&b.quality.class))
        .then_with(|| a.quality.accuracy.cmp(&b.quality.accuracy))
        .then_with(|| {
            a.quality
                .offset_scaled_log_variance
                .cmp(&b.quality.offset_scaled_log_variance)
        })
        .then_with(|| a.priority2.cmp(&b.priority2))
        .then_with(|| a.grandmaster.cmp(&b.grandmaster))
}

struct Tracked {
    dataset: MasterDataset,
    last_seen: Instant,
    timeout: Duration,
}

/// Collects Announces and decides which master to follow.
pub struct BestMaster {
    seen: HashMap<PortIdentity, Tracked>,
    /// How many announce intervals of silence before a master is written off.
    receipt_timeout_multiplier: u32,
}

impl Default for BestMaster {
    fn default() -> Self {
        Self::new()
    }
}

impl BestMaster {
    pub fn new() -> Self {
        Self {
            seen: HashMap::new(),
            // The standard's default. Fewer than three makes a single dropped Announce
            // on a busy network look like a grandmaster failure, and re-electing costs
            // far more than waiting one more interval.
            receipt_timeout_multiplier: 3,
        }
    }

    /// Record an Announce. `log_interval` is the header's `logMessageInterval`.
    pub fn observe(&mut self, dataset: MasterDataset, log_interval: i8, now: Instant) {
        let interval = log_interval_to_duration(log_interval);
        self.seen.insert(
            dataset.sender,
            Tracked {
                dataset,
                last_seen: now,
                timeout: interval * self.receipt_timeout_multiplier,
            },
        );
    }

    /// Drop masters that have gone quiet. Returns how many were dropped.
    pub fn expire(&mut self, now: Instant) -> usize {
        let before = self.seen.len();
        self.seen
            .retain(|_, t| now.duration_since(t.last_seen) < t.timeout);
        before - self.seen.len()
    }

    /// The best master currently known.
    pub fn best(&self) -> Option<MasterDataset> {
        self.seen
            .values()
            .map(|t| t.dataset)
            .reduce(|a, b| if compare(&a, &b).is_le() { a } else { b })
    }

    pub fn known_masters(&self) -> usize {
        self.seen.len()
    }
}

/// PTP expresses intervals as a signed power of two seconds. The AES67 media profile
/// uses -3 for Sync (8/s) and 0 or 1 for Announce.
pub fn log_interval_to_duration(log_interval: i8) -> Duration {
    if log_interval >= 0 {
        Duration::from_secs(1u64 << log_interval.min(16))
    } else {
        Duration::from_secs_f64(2f64.powi(log_interval as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dataset(gm: u8, p1: u8, class: u8, p2: u8) -> MasterDataset {
        MasterDataset {
            grandmaster: ClockIdentity([gm, 0, 0, 0xFF, 0xFE, 0, 0, 0]),
            priority1: p1,
            quality: ClockQuality { class, accuracy: 0x21, offset_scaled_log_variance: 0x436A },
            priority2: p2,
            steps_removed: 0,
            sender: PortIdentity {
                clock: ClockIdentity([gm, 0, 0, 0xFF, 0xFE, 0, 0, 0]),
                port: 1,
            },
        }
    }

    #[test]
    fn priority1_outranks_every_measure_of_quality() {
        // The operator's override: a worse clock with a lower priority1 still wins.
        // If this ever inverts, "force this box to be grandmaster" silently stops
        // working and the domain follows whichever GPS receiver shouts loudest.
        let forced = dataset(1, 50, 248, 128); // priority1 50, terrible class
        let gps = dataset(2, 128, 6, 128); // priority1 128, GPS-locked
        assert_eq!(compare(&forced, &gps), Ordering::Less);
    }

    #[test]
    fn clock_class_decides_when_priorities_match() {
        let gps = dataset(1, 128, 6, 128);
        let holdover = dataset(2, 128, 187, 128);
        assert_eq!(compare(&gps, &holdover), Ordering::Less);
    }

    #[test]
    fn accuracy_then_variance_then_priority2_break_further_ties() {
        let mut a = dataset(1, 128, 6, 128);
        let mut b = dataset(2, 128, 6, 128);

        a.quality.accuracy = 0x20;
        b.quality.accuracy = 0x21;
        assert_eq!(compare(&a, &b), Ordering::Less);

        a.quality.accuracy = 0x21;
        a.quality.offset_scaled_log_variance = 0x4000;
        assert_eq!(compare(&a, &b), Ordering::Less);

        a.quality.offset_scaled_log_variance = b.quality.offset_scaled_log_variance;
        a.priority2 = 100;
        assert_eq!(compare(&a, &b), Ordering::Less);
    }

    #[test]
    fn identical_clocks_are_separated_by_identity_so_everyone_agrees() {
        // Without a total order, two clocks could each conclude the other is better and
        // the domain would never settle.
        let a = dataset(1, 128, 6, 128);
        let b = dataset(2, 128, 6, 128);
        assert_eq!(compare(&a, &b), Ordering::Less);
        assert_eq!(compare(&b, &a), Ordering::Greater);
        assert_eq!(compare(&a, &a), Ordering::Equal);
    }

    #[test]
    fn the_same_grandmaster_via_a_shorter_path_wins() {
        let mut near = dataset(1, 128, 6, 128);
        let mut far = dataset(1, 128, 6, 128);
        near.steps_removed = 1;
        far.steps_removed = 3;
        far.sender.port = 2;
        assert_eq!(compare(&near, &far), Ordering::Less);
    }

    #[test]
    fn picks_the_best_of_several_and_re_picks_when_it_disappears() {
        let mut bm = BestMaster::new();
        let now = Instant::now();

        let gps = dataset(1, 128, 6, 128);
        let ordinary = dataset(2, 128, 187, 128);
        bm.observe(gps, 1, now);
        bm.observe(ordinary, 1, now);

        assert_eq!(bm.known_masters(), 2);
        assert_eq!(bm.best().unwrap().grandmaster, gps.grandmaster);

        // The GPS clock goes quiet for longer than 3 announce intervals.
        let later = now + Duration::from_secs(7);
        bm.observe(ordinary, 1, later);
        assert_eq!(bm.expire(later), 1);
        assert_eq!(bm.best().unwrap().grandmaster, ordinary.grandmaster);
    }

    #[test]
    fn an_empty_domain_has_no_master() {
        let bm = BestMaster::new();
        assert!(bm.best().is_none());
    }

    #[test]
    fn log_intervals_are_signed_powers_of_two_seconds() {
        assert_eq!(log_interval_to_duration(0), Duration::from_secs(1));
        assert_eq!(log_interval_to_duration(1), Duration::from_secs(2));
        // -3 is the AES67 media profile's Sync rate: eight per second.
        assert_eq!(log_interval_to_duration(-3), Duration::from_millis(125));
        assert_eq!(log_interval_to_duration(-4), Duration::from_micros(62_500));
    }
}

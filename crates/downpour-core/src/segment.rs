//! Byte-range bookkeeping for multi-connection downloads.
//!
//! The strategy mirrors the classic "dynamic segmentation" used by desktop
//! download managers: the first connection starts on the whole file, and every
//! additional connection takes the second half of whichever segment still has
//! the most unclaimed bytes. A connection that finishes early therefore helps
//! the slowest one instead of sitting idle, and no fixed split is ever wrong.
//!
//! Near the end no segment is big enough to halve, and a server can slow one
//! connection to a trickle (a few KB/s: never slow enough for the read
//! timeout). A connection measured that slow hands all but a second's worth
//! of its range to a new connection, so the last piece cannot hold the
//! download at 99% for minutes.
//!
//! A split is tentative until the server accepts the new connection: the
//! running connection keeps its whole range meanwhile. Servers that refuse
//! new requests late in a download (some download links do) would
//! otherwise leave the second half with nobody able to fetch it, while the
//! running connection could have finished it over the stream it has.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// A persisted byte range `[start, end)` of the target file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    /// Bytes handed to the OS starting at `start`. Resume continues from here.
    pub written: u64,
}

impl Segment {
    pub fn write_position(&self) -> u64 {
        self.start + self.written
    }

    pub fn remaining(&self) -> u64 {
        self.end - self.write_position()
    }

    pub fn is_complete(&self) -> bool {
        self.write_position() >= self.end
    }
}

/// Stable handle for a segment; indices never shift when segments split.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SlotId(usize);

/// Where a slot made by a split stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Split {
    /// A normal slot: its range is its own.
    Settled,
    /// The second half of `donor`, whose connection still owns the range
    /// until the new connection is accepted.
    Tentative { donor: usize },
    /// The new connection was refused or failed; the range stayed with the
    /// donor. The slot is kept so ids stay stable, and is ignored.
    Withdrawn,
}

/// What the new connection of a split does once the server accepted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitConfirmation {
    /// Download the slot's range, first discarding this many bytes of the
    /// response: the donor claimed them while the request was on its way.
    Proceed { skip: u64 },
    /// The donor already claimed the whole range; close the connection.
    NothingLeft,
}

/// How long a connection's speed is measured, from its first bytes, before
/// it counts.
const PACE_WINDOW: Duration = Duration::from_secs(2);
/// A connection that would need longer than this for the rest of its range
/// hands the range over.
const SLOW_TAIL_PATIENCE: Duration = Duration::from_secs(5);
/// What a slow connection keeps when it hands its range over: about this
/// much of its own time, so the bytes it has in flight are not wasted.
const SLOW_TAIL_KEEP: Duration = Duration::from_secs(1);
/// The least a slow connection keeps, and the least it hands over.
const SLOW_TAIL_MIN_PIECE: u64 = 16 * 1024;

/// A connection's speed over its latest measuring window.
#[derive(Clone, Copy, Debug)]
struct Pace {
    window_start: Instant,
    claimed_at_start: u64,
    /// Bytes per second over the last full window.
    last_rate: Option<u64>,
}

impl Pace {
    fn starting(now: Instant, claimed_to: u64) -> Self {
        Self {
            window_start: now,
            claimed_at_start: claimed_to,
            last_rate: None,
        }
    }

    /// The speed over the current window once it is long enough to count,
    /// so a connection that stopped receiving reads as slow; until then the
    /// last full window's.
    fn rate(&self, claimed_to: u64, now: Instant) -> Option<u64> {
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed < PACE_WINDOW {
            return self.last_rate;
        }
        let bytes = claimed_to.saturating_sub(self.claimed_at_start);
        Some((bytes as f64 / elapsed.as_secs_f64()) as u64)
    }

    fn record(&mut self, claimed_to: u64, now: Instant) {
        if now.saturating_duration_since(self.window_start) >= PACE_WINDOW {
            *self = Pace {
                last_rate: self.rate(claimed_to, now),
                ..Pace::starting(now, claimed_to)
            };
        }
    }
}

#[derive(Debug)]
struct Slot {
    segment: Segment,
    split: Split,
    /// Absolute offset up to which a connection has accepted bytes. Bytes in
    /// `write_position..claimed_to` are buffered in memory, not yet on disk.
    claimed_to: u64,
    assigned: bool,
    /// The speed of the slot's connection, measured from its first bytes.
    pace: Option<Pace>,
    consecutive_failures: u32,
    retry_not_before: Option<Instant>,
}

impl Slot {
    fn new(segment: Segment) -> Self {
        Self {
            claimed_to: segment.write_position(),
            segment,
            split: Split::Settled,
            assigned: false,
            pace: None,
            consecutive_failures: 0,
            retry_not_before: None,
        }
    }

    fn unclaimed(&self) -> u64 {
        self.segment.end.saturating_sub(self.claimed_to)
    }

    fn is_ready_for_retry(&self, now: Instant) -> bool {
        self.retry_not_before.is_none_or(|at| now >= at)
    }

    /// Whether the slot's range is its own (not tentative or withdrawn).
    fn is_settled(&self) -> bool {
        self.split == Split::Settled
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SegmentLayoutError {
    #[error("segments do not cover 0..{size} contiguously")]
    NotContiguous { size: u64 },
    #[error("segment {start}..{end} claims {written} written bytes")]
    WrittenPastEnd { start: u64, end: u64, written: u64 },
}

/// Live view of which bytes are written, in flight, or still unassigned.
#[derive(Debug)]
pub struct SegmentTable {
    slots: Vec<Slot>,
    size: u64,
}

impl SegmentTable {
    /// One segment covering the whole file, for the first connection.
    pub fn whole_file(size: u64) -> Self {
        Self {
            slots: vec![Slot::new(Segment {
                start: 0,
                end: size,
                written: 0,
            })],
            size,
        }
    }

    /// Rebuilds the table from persisted segments, rejecting layouts that
    /// would leave holes or overlaps in the file.
    pub fn from_saved(size: u64, mut segments: Vec<Segment>) -> Result<Self, SegmentLayoutError> {
        segments.sort_by_key(|segment| segment.start);
        let mut expected_start = 0;
        for segment in &segments {
            if segment.start != expected_start || segment.end < segment.start {
                return Err(SegmentLayoutError::NotContiguous { size });
            }
            if segment.write_position() > segment.end {
                return Err(SegmentLayoutError::WrittenPastEnd {
                    start: segment.start,
                    end: segment.end,
                    written: segment.written,
                });
            }
            expected_start = segment.end;
        }
        if expected_start != size {
            return Err(SegmentLayoutError::NotContiguous { size });
        }
        Ok(Self {
            slots: segments.into_iter().map(Slot::new).collect(),
            size,
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn written_bytes(&self) -> u64 {
        self.settled().map(|slot| slot.segment.written).sum()
    }

    pub fn is_complete(&self) -> bool {
        self.settled().all(|slot| slot.segment.is_complete())
    }

    pub fn assigned_count(&self) -> usize {
        self.slots.iter().filter(|slot| slot.assigned).count()
    }

    /// Slots that own their range; together they cover the file exactly.
    fn settled(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter().filter(|slot| slot.is_settled())
    }

    /// Segments ordered by file offset, for persistence and display.
    pub fn segments(&self) -> Vec<Segment> {
        let mut segments: Vec<Segment> = self.settled().map(|slot| slot.segment).collect();
        segments.sort_by_key(|segment| segment.start);
        segments
    }

    pub fn segment(&self, slot: SlotId) -> Segment {
        self.slots[slot.0].segment
    }

    /// Chooses work for one new connection: an idle unfinished segment first,
    /// otherwise the second half of the assigned segment with the most
    /// unclaimed bytes, as a tentative split (see [`Self::confirm_split`]).
    /// When no segment can be halved without making one smaller than
    /// `min_split`, a connection too slow to finish its range soon hands it
    /// over (see the module notes). Returns `None` when there is no work.
    pub fn assign_next(&mut self, now: Instant, min_split: u64) -> Option<SlotId> {
        if let Some(index) = self.slots.iter().position(|slot| {
            slot.is_settled()
                && !slot.assigned
                && !slot.segment.is_complete()
                && slot.is_ready_for_retry(now)
        }) {
            self.slots[index].assigned = true;
            return Some(SlotId(index));
        }
        self.halve_largest(min_split)
            .or_else(|| self.hand_over_slow_tail(now))
    }

    /// Slots that a new connection may take part of: running, owning their
    /// range, and with no split of theirs still waiting for the server.
    fn donors(&self) -> impl Iterator<Item = (usize, &Slot)> {
        let waiting: Vec<usize> = self
            .slots
            .iter()
            .filter_map(|slot| match slot.split {
                Split::Tentative { donor } => Some(donor),
                Split::Settled | Split::Withdrawn => None,
            })
            .collect();
        self.slots.iter().enumerate().filter(move |(index, slot)| {
            slot.assigned && slot.is_settled() && !waiting.contains(index)
        })
    }

    /// The second half of the segment with the most unclaimed bytes, unless
    /// a half would be smaller than `min_split`.
    fn halve_largest(&mut self, min_split: u64) -> Option<SlotId> {
        let (donor, unclaimed) = self
            .donors()
            .map(|(index, slot)| (index, slot.unclaimed()))
            .max_by_key(|(_, unclaimed)| *unclaimed)?;
        if unclaimed < min_split.saturating_mul(2) {
            return None;
        }
        let split_at = self.slots[donor].claimed_to + unclaimed / 2;
        Some(self.split_off(donor, split_at))
    }

    /// All but about a second's worth of the range of the connection that
    /// would take longest to finish, if that is longer than
    /// `SLOW_TAIL_PATIENCE` at its measured speed.
    fn hand_over_slow_tail(&mut self, now: Instant) -> Option<SlotId> {
        let (donor, keep, _) = self
            .donors()
            .filter_map(|(index, slot)| {
                let rate = slot.pace?.rate(slot.claimed_to, now)?;
                let keep = slow_tail_keep(rate);
                let unclaimed = slot.unclaimed();
                let finishes_soon =
                    (unclaimed as f64) <= rate as f64 * SLOW_TAIL_PATIENCE.as_secs_f64();
                (!finishes_soon && unclaimed >= keep + SLOW_TAIL_MIN_PIECE)
                    .then_some((index, keep, unclaimed))
            })
            .max_by_key(|(_, _, unclaimed)| *unclaimed)?;
        let split_at = self.slots[donor].claimed_to + keep;
        Some(self.split_off(donor, split_at))
    }

    /// A tentative new slot for `donor`'s range from `split_at` on.
    fn split_off(&mut self, donor: usize, split_at: u64) -> SlotId {
        let mut tail = Slot::new(Segment {
            start: split_at,
            end: self.slots[donor].segment.end,
            written: 0,
        });
        tail.split = Split::Tentative { donor };
        tail.assigned = true;
        self.slots.push(tail);
        SlotId(self.slots.len() - 1)
    }

    /// The server accepted the new connection of a split: the donor gives up
    /// the range it has not claimed yet. Settled slots proceed unchanged.
    pub fn confirm_split(&mut self, slot: SlotId) -> SplitConfirmation {
        let Split::Tentative { donor } = self.slots[slot.0].split else {
            return SplitConfirmation::Proceed { skip: 0 };
        };
        let requested_from = self.slots[slot.0].segment.start;
        let end = self.slots[slot.0].segment.end;
        let donor_claimed = self.slots[donor].claimed_to;
        if donor_claimed >= end {
            let tail = &mut self.slots[slot.0];
            tail.split = Split::Withdrawn;
            tail.assigned = false;
            return SplitConfirmation::NothingLeft;
        }
        let start = requested_from.max(donor_claimed);
        self.slots[donor].segment.end = start;
        let tail = &mut self.slots[slot.0];
        tail.segment.start = start;
        tail.claimed_to = start;
        tail.split = Split::Settled;
        SplitConfirmation::Proceed {
            skip: start - requested_from,
        }
    }

    /// Accepts up to `available` incoming bytes for `slot`, never past the
    /// segment's current end (which may have shrunk after a split). Returns
    /// how many of the bytes the connection may keep.
    pub fn claim(&mut self, slot: SlotId, available: u64, now: Instant) -> u64 {
        let slot = &mut self.slots[slot.0];
        let accepted = available.min(slot.unclaimed());
        slot.claimed_to += accepted;
        if accepted > 0 {
            // Timed from the first bytes: connecting is not slow sending.
            let claimed_before = slot.claimed_to - accepted;
            slot.pace
                .get_or_insert_with(|| Pace::starting(now, claimed_before))
                .record(slot.claimed_to, now);
            slot.consecutive_failures = 0;
        }
        accepted
    }

    /// True once every byte of the segment has been claimed by its connection.
    pub fn is_fully_claimed(&self, slot: SlotId) -> bool {
        self.slots[slot.0].unclaimed() == 0
    }

    pub fn record_written(&mut self, slot: SlotId, bytes: u64) {
        let slot = &mut self.slots[slot.0];
        slot.segment.written += bytes;
        debug_assert!(slot.segment.write_position() <= slot.claimed_to);
    }

    /// Returns the segment to the pool after its connection ended. Claimed but
    /// unwritten bytes are forgotten so the next connection refetches them.
    pub fn release(&mut self, slot: SlotId) {
        let slot = &mut self.slots[slot.0];
        slot.assigned = false;
        slot.pace = None;
        if let Split::Tentative { .. } = slot.split {
            // Never accepted: the range stays with the donor.
            slot.split = Split::Withdrawn;
            return;
        }
        slot.claimed_to = slot.segment.write_position();
    }

    /// Releases the segment and delays its reassignment with exponential
    /// backoff. Returns the number of consecutive failures so far.
    pub fn release_after_failure(&mut self, slot: SlotId, now: Instant) -> u32 {
        self.release(slot);
        let slot = &mut self.slots[slot.0];
        if slot.split == Split::Withdrawn {
            // Nothing to retry: the donor still covers the range.
            return 0;
        }
        slot.consecutive_failures += 1;
        slot.retry_not_before = Some(now + retry_backoff(slot.consecutive_failures));
        slot.consecutive_failures
    }
}

/// What a slow connection keeps of its range: about `SLOW_TAIL_KEEP` of its
/// own time.
fn slow_tail_keep(bytes_per_second: u64) -> u64 {
    let in_keep_time = bytes_per_second as f64 * SLOW_TAIL_KEEP.as_secs_f64();
    (in_keep_time as u64).max(SLOW_TAIL_MIN_PIECE)
}

/// 0.5 s, 1 s, 2 s, ... capped at 30 s.
fn retry_backoff(consecutive_failures: u32) -> std::time::Duration {
    let millis = 500u64 << consecutive_failures.saturating_sub(1).min(6);
    std::time::Duration::from_millis(millis.min(30_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;

    /// A two-connection download near its end: the first connection holds
    /// the last 600 KiB, too little to halve with a 1 MiB minimum.
    fn tail_on_one_connection(start: Instant) -> (SegmentTable, SlotId) {
        let size = 10 * MIB;
        let mut table = SegmentTable::from_saved(
            size,
            vec![
                Segment {
                    start: 0,
                    end: size - 600 * KIB,
                    written: size - 600 * KIB,
                },
                Segment {
                    start: size - 600 * KIB,
                    end: size,
                    written: 0,
                },
            ],
        )
        .unwrap();
        let slow = table.assign_next(start, MIB).unwrap();
        (table, slow)
    }

    #[test]
    fn a_trickling_last_piece_is_handed_to_a_new_connection() {
        let start = Instant::now();
        let (mut table, slow) = tail_on_one_connection(start);
        // About 6 KB/s, as a server that throttles one connection sends.
        table.claim(slow, 2 * KIB, start);
        let measured = start + Duration::from_secs(3);
        table.claim(slow, 16 * KIB, measured);

        let helper = table
            .assign_next(measured, MIB)
            .expect("the slow tail is handed over");

        let kept_to = table.segment(slow).start + 18 * KIB + SLOW_TAIL_MIN_PIECE;
        assert_eq!(table.segment(helper).start, kept_to);
        assert_eq!(
            table.confirm_split(helper),
            SplitConfirmation::Proceed { skip: 0 }
        );
        assert_eq!(table.segment(slow).end, kept_to);
    }

    #[test]
    fn a_connection_that_stops_receiving_hands_over_its_range() {
        let start = Instant::now();
        let (mut table, slow) = tail_on_one_connection(start);
        table.claim(slow, 2 * KIB, start);
        assert!(table
            .assign_next(start + Duration::from_secs(3), MIB)
            .is_some());
    }

    #[test]
    fn a_fast_last_piece_keeps_its_connection() {
        let start = Instant::now();
        let (mut table, slow) = tail_on_one_connection(start);
        table.claim(slow, KIB, start);
        let measured = start + Duration::from_secs(2);
        // 200 KiB/s finishes the rest well within the patience.
        table.claim(slow, 400 * KIB, measured);
        assert_eq!(table.assign_next(measured, MIB), None);
    }

    #[test]
    fn a_new_connection_is_not_judged_before_it_is_measured() {
        let start = Instant::now();
        let (mut table, slow) = tail_on_one_connection(start);
        table.claim(slow, KIB, start);
        assert_eq!(table.assign_next(start + Duration::from_secs(1), MIB), None);
    }

    #[test]
    fn time_spent_connecting_does_not_count_as_slow() {
        let start = Instant::now();
        let (mut table, slow) = tail_on_one_connection(start);
        // The first bytes arrive 1.5 s after the request: a slow handshake.
        let first_bytes = start + Duration::from_millis(1500);
        table.claim(slow, 50 * KIB, first_bytes);
        // Half a second of receiving is too short to judge, however long
        // the connection took to start.
        let soon_after = first_bytes + Duration::from_millis(500);
        table.claim(slow, 50 * KIB, soon_after);
        assert_eq!(table.assign_next(soon_after, MIB), None);
    }

    const MIB: u64 = 1024 * 1024;

    /// Starts the first connection and splits off a second, returning both.
    fn split_two(size: u64) -> (SegmentTable, SlotId, SlotId) {
        let mut table = SegmentTable::whole_file(size);
        let first = table.assign_next(Instant::now(), MIB).unwrap();
        let second = table.assign_next(Instant::now(), MIB).unwrap();
        (table, first, second)
    }

    #[test]
    fn a_split_waits_for_the_new_connection_to_be_accepted() {
        let (mut table, first, second) = split_two(100 * MIB);
        // Until accepted, the first connection still owns the whole file.
        assert_eq!(table.segment(first).end, 100 * MIB);
        assert_eq!(table.segments().len(), 1);
        assert_eq!(
            table.confirm_split(second),
            SplitConfirmation::Proceed { skip: 0 }
        );
        assert_eq!(table.segment(first).end, 50 * MIB);
        assert_eq!(table.segment(second).start, 50 * MIB);
        assert_eq!(table.segments().len(), 2);
    }

    #[test]
    fn a_refused_split_leaves_the_range_with_the_running_connection() {
        let (mut table, first, second) = split_two(100 * MIB);
        assert_eq!(table.release_after_failure(second, Instant::now()), 0);
        assert_eq!(table.segment(first).end, 100 * MIB);
        // The running connection can still claim every byte.
        assert_eq!(table.claim(first, 100 * MIB, Instant::now()), 100 * MIB);
        table.record_written(first, 100 * MIB);
        assert!(table.is_complete());
        // The withdrawn half is never handed out again.
        assert!(table.assign_next(Instant::now(), MIB).is_none());
    }

    #[test]
    fn bytes_the_donor_claimed_meanwhile_are_skipped() {
        let (mut table, first, second) = split_two(100 * MIB);
        table.claim(first, 60 * MIB, Instant::now());
        assert_eq!(
            table.confirm_split(second),
            SplitConfirmation::Proceed { skip: 10 * MIB }
        );
        assert_eq!(table.segment(first).end, 60 * MIB);
        assert_eq!(table.segment(second).start, 60 * MIB);
    }

    #[test]
    fn a_split_the_donor_finished_has_nothing_left() {
        let (mut table, first, second) = split_two(100 * MIB);
        table.claim(first, 100 * MIB, Instant::now());
        assert_eq!(table.confirm_split(second), SplitConfirmation::NothingLeft);
        assert_eq!(table.segment(first).end, 100 * MIB);
    }

    #[test]
    fn first_connection_owns_whole_file() {
        let mut table = SegmentTable::whole_file(100 * MIB);
        let first = table.assign_next(Instant::now(), MIB).unwrap();
        assert_eq!(
            table.segment(first),
            Segment {
                start: 0,
                end: 100 * MIB,
                written: 0
            }
        );
    }

    #[test]
    fn new_connection_splits_largest_unclaimed_segment_in_half() {
        let now = Instant::now();
        let mut table = SegmentTable::whole_file(100 * MIB);
        let first = table.assign_next(now, MIB).unwrap();
        table.claim(first, 20 * MIB, Instant::now());

        let second = table.assign_next(now, MIB).unwrap();
        table.confirm_split(second);

        assert_eq!(table.segment(first).end, 60 * MIB);
        assert_eq!(
            table.segment(second),
            Segment {
                start: 60 * MIB,
                end: 100 * MIB,
                written: 0
            }
        );
        assert_eq!(
            table
                .segments()
                .iter()
                .map(|s| s.end - s.start)
                .sum::<u64>(),
            100 * MIB
        );
    }

    #[test]
    fn claim_stops_at_shrunk_end() {
        let now = Instant::now();
        let mut table = SegmentTable::whole_file(10 * MIB);
        let first = table.assign_next(now, MIB).unwrap();
        let second = table.assign_next(now, MIB).unwrap();
        table.confirm_split(second);

        assert_eq!(table.claim(first, 8 * MIB, Instant::now()), 5 * MIB);
        assert!(table.is_fully_claimed(first));
    }

    #[test]
    fn refuses_splits_below_minimum() {
        let now = Instant::now();
        let mut table = SegmentTable::whole_file(MIB);
        table.assign_next(now, MIB).unwrap();
        assert_eq!(table.assign_next(now, MIB), None);
    }

    #[test]
    fn idle_unfinished_segment_is_reused_before_splitting() {
        let now = Instant::now();
        let mut table = SegmentTable::whole_file(10 * MIB);
        let first = table.assign_next(now, MIB).unwrap();
        table.claim(first, MIB, Instant::now());
        table.record_written(first, MIB);
        table.release(first);

        assert_eq!(table.assign_next(now, MIB), Some(first));
        assert_eq!(table.segment(first).write_position(), MIB);
    }

    #[test]
    fn failed_segment_waits_for_backoff() {
        let now = Instant::now();
        let mut table = SegmentTable::whole_file(MIB);
        let first = table.assign_next(now, MIB).unwrap();
        assert_eq!(table.release_after_failure(first, now), 1);

        assert_eq!(table.assign_next(now, MIB), None);
        assert_eq!(
            table.assign_next(now + std::time::Duration::from_millis(600), MIB),
            Some(first)
        );
    }

    #[test]
    fn retry_backoff_grows_and_caps() {
        assert_eq!(retry_backoff(1).as_millis(), 500);
        assert_eq!(retry_backoff(3).as_millis(), 2000);
        assert_eq!(retry_backoff(20).as_millis(), 30_000);
    }

    #[test]
    fn release_forgets_unwritten_claims() {
        let now = Instant::now();
        let mut table = SegmentTable::whole_file(10 * MIB);
        let first = table.assign_next(now, MIB).unwrap();
        table.claim(first, 4 * MIB, Instant::now());
        table.record_written(first, MIB);
        table.release(first);

        let again = table.assign_next(now, MIB).unwrap();
        assert_eq!(table.claim(again, 100 * MIB, Instant::now()), 9 * MIB);
    }

    #[test]
    fn saved_layout_must_be_contiguous() {
        let gap = vec![
            Segment {
                start: 0,
                end: 10,
                written: 10,
            },
            Segment {
                start: 12,
                end: 20,
                written: 0,
            },
        ];
        assert_eq!(
            SegmentTable::from_saved(20, gap).unwrap_err(),
            SegmentLayoutError::NotContiguous { size: 20 }
        );

        let valid = vec![
            Segment {
                start: 10,
                end: 20,
                written: 3,
            },
            Segment {
                start: 0,
                end: 10,
                written: 10,
            },
        ];
        let table = SegmentTable::from_saved(20, valid).unwrap();
        assert_eq!(table.written_bytes(), 13);
        assert!(!table.is_complete());
    }
}

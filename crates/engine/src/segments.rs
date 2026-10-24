//! Dynamic segmentation, the heart of the accelerator.
//!
//! A file is a sorted list of non-overlapping segments `[start, end)` that
//! together cover the whole file. Segments are never planned up front:
//!
//! * When a connection becomes free it first takes any segment nobody owns
//!   (left over from a resume or from a failed connection).
//! * Otherwise it finds the segment with the most bytes still to receive and
//!   splits it in half, taking the back half for itself. The original owner
//!   keeps downloading the front half and simply stops earlier.
//! * When a connection reaches the end of its segment and the next segment's
//!   owner has not received a single byte yet, it absorbs that segment and
//!   keeps reading from the same HTTP response, which saves a round trip.
//!
//! Each segment tracks two positions: `received` (bytes that have arrived
//! from the network, used for split decisions) and `written` (bytes that are
//! on disk, the only thing that is ever persisted for resume).

use serde::{Deserialize, Serialize};

pub type WorkerId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub id: u64,
    pub start: u64,
    pub end: u64,
    pub received: u64,
    pub written: u64,
    pub owner: Option<WorkerId>,
}

/// The persisted form of a segment: what is on disk, nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSegment {
    pub start: u64,
    pub end: u64,
    pub written: u64,
}

/// Work handed to a connection: request bytes `from..to` for segment `id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    pub id: u64,
    pub from: u64,
    pub to: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receive {
    /// Keep the first `keep` bytes of the chunk. `done` means the segment is
    /// fully received and the rest of the response must be discarded.
    Keep { keep: u64, done: bool },
    /// The segment was absorbed by a neighbour before this connection
    /// received anything; drop the response and ask for new work.
    Abandon,
}

#[derive(Debug)]
pub struct SegmentMap {
    segs: Vec<Segment>,
    next_id: u64,
    min_split: u64,
}

impl SegmentMap {
    pub fn new(total: u64, min_split: u64) -> Self {
        let saved = if total == 0 {
            Vec::new()
        } else {
            vec![SavedSegment {
                start: 0,
                end: total,
                written: 0,
            }]
        };
        Self::from_saved(&saved, min_split)
    }

    pub fn from_saved(saved: &[SavedSegment], min_split: u64) -> Self {
        let mut segs: Vec<Segment> = saved
            .iter()
            .enumerate()
            .map(|(i, s)| Segment {
                id: i as u64,
                start: s.start,
                end: s.end,
                received: s.written,
                written: s.written,
                owner: None,
            })
            .collect();
        segs.sort_by_key(|s| s.start);
        Self {
            next_id: segs.len() as u64,
            segs,
            min_split: min_split.max(1),
        }
    }

    /// Checks that saved segments exactly tile `0..total` with sane positions.
    pub fn validate_saved(saved: &[SavedSegment], total: u64) -> bool {
        let mut sorted = saved.to_vec();
        sorted.sort_by_key(|s| s.start);
        let mut cursor = 0;
        for s in &sorted {
            if s.start != cursor || s.end <= s.start || s.written < s.start || s.written > s.end {
                return false;
            }
            cursor = s.end;
        }
        cursor == total
    }

    pub fn saved(&self) -> Vec<SavedSegment> {
        self.segs
            .iter()
            .map(|s| SavedSegment {
                start: s.start,
                end: s.end,
                written: s.written,
            })
            .collect()
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segs
    }

    pub fn is_complete(&self) -> bool {
        self.segs.iter().all(|s| s.written == s.end)
    }

    pub fn written_bytes(&self) -> u64 {
        self.segs.iter().map(|s| s.written - s.start).sum()
    }

    pub fn active_connections(&self) -> usize {
        self.segs.iter().filter(|s| s.owner.is_some()).count()
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.segs.iter().position(|s| s.id == id)
    }

    /// Finds work for a free connection, or `None` if there is nothing left
    /// worth splitting.
    pub fn claim(&mut self, worker: WorkerId) -> Option<Assignment> {
        if let Some(seg) = self
            .segs
            .iter_mut()
            .find(|s| s.owner.is_none() && s.written < s.end)
        {
            seg.owner = Some(worker);
            seg.received = seg.written;
            return Some(Assignment {
                id: seg.id,
                from: seg.written,
                to: seg.end,
            });
        }

        let (i, remaining) = self
            .segs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.owner.is_some())
            .map(|(i, s)| (i, s.end - s.received))
            .max_by_key(|&(_, remaining)| remaining)?;
        if remaining < 2 * self.min_split {
            return None;
        }

        let old = &mut self.segs[i];
        let mid = old.received + remaining / 2;
        let tail = Segment {
            id: self.next_id,
            start: mid,
            end: old.end,
            received: mid,
            written: mid,
            owner: Some(worker),
        };
        old.end = mid;
        self.next_id += 1;
        let assignment = Assignment {
            id: tail.id,
            from: tail.start,
            to: tail.end,
        };
        self.segs.insert(i + 1, tail);
        Some(assignment)
    }

    /// Records `n` bytes arriving for segment `id` on a response that will
    /// deliver bytes up to `request_end` (exclusive).
    pub fn on_receive(&mut self, worker: WorkerId, id: u64, n: u64, request_end: u64) -> Receive {
        let i = match self.index_of(id) {
            Some(i) if self.segs[i].owner == Some(worker) => i,
            _ => return Receive::Abandon,
        };
        let mut avail = n;
        let mut keep = 0;
        loop {
            let seg = &mut self.segs[i];
            let take = (seg.end - seg.received).min(avail);
            seg.received += take;
            keep += take;
            avail -= take;
            if seg.received < seg.end {
                return Receive::Keep { keep, done: false };
            }
            if !self.try_absorb_next(i, request_end) {
                return Receive::Keep { keep, done: true };
            }
            if avail == 0 {
                return Receive::Keep { keep, done: false };
            }
        }
    }

    /// Lets segment `i` swallow its neighbour if that neighbour has not
    /// received anything yet and lies within the current response.
    fn try_absorb_next(&mut self, i: usize, request_end: u64) -> bool {
        let Some(next) = self.segs.get(i + 1) else {
            return false;
        };
        if next.start != self.segs[i].end || next.received != next.start || next.end > request_end {
            return false;
        }
        self.segs[i].end = next.end;
        self.segs.remove(i + 1);
        true
    }

    /// Records that bytes up to `upto` are now on disk for segment `id`.
    pub fn on_written(&mut self, id: u64, upto: u64) {
        if let Some(i) = self.index_of(id) {
            let seg = &mut self.segs[i];
            seg.written = upto.clamp(seg.written, seg.end);
        }
    }

    /// Gives a segment back, e.g. after an error or when its work is done.
    /// Anything received but not written is forgotten and will be re-fetched.
    pub fn release(&mut self, worker: WorkerId, id: u64) {
        if let Some(i) = self.index_of(id) {
            let seg = &mut self.segs[i];
            if seg.owner == Some(worker) {
                seg.owner = None;
                seg.received = seg.written;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    fn receive_all(map: &mut SegmentMap, w: WorkerId, a: Assignment) {
        let n = a.to - a.from;
        assert_eq!(
            map.on_receive(w, a.id, n, a.to),
            Receive::Keep {
                keep: n,
                done: true
            }
        );
        map.on_written(a.id, a.to);
        map.release(w, a.id);
    }

    #[test]
    fn first_claim_takes_whole_file() {
        let mut map = SegmentMap::new(10 * MB, MB);
        assert_eq!(
            map.claim(0),
            Some(Assignment {
                id: 0,
                from: 0,
                to: 10 * MB
            })
        );
    }

    #[test]
    fn free_connection_splits_largest_segment_in_half() {
        let mut map = SegmentMap::new(10 * MB, MB);
        let a = map.claim(0).unwrap();
        map.on_receive(0, a.id, 2 * MB, a.to);
        // 8 MB left on segment 0, so the split lands 4 MB past the received point.
        let b = map.claim(1).unwrap();
        assert_eq!((b.from, b.to), (6 * MB, 10 * MB));
        assert_eq!(map.segments()[0].end, 6 * MB);
        // The next split goes to the bigger of the two remaining halves (4 MB each,
        // ties go to the later segment).
        let c = map.claim(2).unwrap();
        assert_eq!((c.from, c.to), (8 * MB, 10 * MB));
    }

    #[test]
    fn does_not_split_below_minimum() {
        let mut map = SegmentMap::new(3 * MB, 2 * MB);
        map.claim(0).unwrap();
        assert_eq!(map.claim(1), None);
    }

    #[test]
    fn absorbs_neighbour_that_has_not_started() {
        let mut map = SegmentMap::new(4 * MB, MB);
        let a = map.claim(0).unwrap();
        let b = map.claim(1).unwrap(); // 2..4 MB, still connecting
        // Segment 0 reaches 2 MB and keeps going on the same response.
        assert_eq!(
            map.on_receive(0, a.id, 3 * MB, a.to),
            Receive::Keep {
                keep: 3 * MB,
                done: false
            }
        );
        assert_eq!(map.segments().len(), 1);
        assert_eq!(map.segments()[0].end, 4 * MB);
        // The other connection finds its segment gone.
        assert_eq!(map.on_receive(1, b.id, 1, b.to), Receive::Abandon);
    }

    #[test]
    fn does_not_absorb_neighbour_that_started() {
        let mut map = SegmentMap::new(4 * MB, MB);
        let a = map.claim(0).unwrap();
        let b = map.claim(1).unwrap();
        map.on_receive(1, b.id, 1, b.to);
        assert_eq!(
            map.on_receive(0, a.id, 3 * MB, a.to),
            Receive::Keep {
                keep: 2 * MB,
                done: true
            }
        );
        assert_eq!(map.segments().len(), 2);
    }

    #[test]
    fn does_not_absorb_beyond_current_response() {
        // After a resume, segment 1 is unowned but lies past what segment 0's
        // request will deliver.
        let saved = [
            SavedSegment {
                start: 0,
                end: 2 * MB,
                written: 0,
            },
            SavedSegment {
                start: 2 * MB,
                end: 4 * MB,
                written: 2 * MB,
            },
        ];
        let mut map = SegmentMap::from_saved(&saved, MB);
        let a = map.claim(0).unwrap();
        assert_eq!(
            map.on_receive(0, a.id, 2 * MB, a.to),
            Receive::Keep {
                keep: 2 * MB,
                done: true
            }
        );
    }

    #[test]
    fn released_segment_is_claimed_first_from_written_position() {
        let mut map = SegmentMap::new(8 * MB, MB);
        let a = map.claim(0).unwrap();
        map.on_receive(0, a.id, 3 * MB, a.to);
        map.on_written(a.id, 2 * MB);
        map.release(0, a.id); // e.g. the connection dropped
        assert_eq!(
            map.claim(1),
            Some(Assignment {
                id: a.id,
                from: 2 * MB,
                to: 8 * MB
            })
        );
    }

    #[test]
    fn completes_and_round_trips_through_saved_form() {
        let mut map = SegmentMap::new(8 * MB, MB);
        let a = map.claim(0).unwrap();
        let b = map.claim(1).unwrap();
        receive_all(&mut map, 1, b);
        assert!(!map.is_complete());
        let saved = map.saved();
        assert!(SegmentMap::validate_saved(&saved, 8 * MB));

        let mut resumed = SegmentMap::from_saved(&saved, MB);
        assert_eq!(resumed.written_bytes(), 4 * MB);
        let a2 = resumed.claim(0).unwrap();
        assert_eq!((a2.from, a2.to), (a.from, 4 * MB));
        receive_all(&mut resumed, 0, a2);
        assert!(resumed.is_complete());
        assert_eq!(resumed.claim(0), None);
    }

    #[test]
    fn rejects_corrupt_saved_segments() {
        let gap = [
            SavedSegment {
                start: 0,
                end: 5,
                written: 0,
            },
            SavedSegment {
                start: 6,
                end: 10,
                written: 6,
            },
        ];
        assert!(!SegmentMap::validate_saved(&gap, 10));
        let short = [SavedSegment {
            start: 0,
            end: 5,
            written: 0,
        }];
        assert!(!SegmentMap::validate_saved(&short, 10));
        let bad_pos = [SavedSegment {
            start: 0,
            end: 10,
            written: 11,
        }];
        assert!(!SegmentMap::validate_saved(&bad_pos, 10));
    }
}

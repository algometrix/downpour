//! The order downloads start in, which the user can rearrange.
//!
//! Every download has a position; lower starts first. Queue views show a
//! subset of the same order, so moving a download relative to its visible
//! neighbors keeps its place among downloads shown elsewhere.

use crate::record::DownloadId;

/// Where moved downloads go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuePlace {
    /// Just ahead of this download.
    Before(DownloadId),
    /// After every other download.
    End,
}

/// `order` with the downloads in `moved` taken out and put together at
/// `place`, in the order they had in `order`.
///
/// A place before one of the moved downloads means before the first download
/// after it that stays put, so dropping a group just below itself changes
/// nothing. A place relative to a download not in `order` leaves it
/// unchanged, and so does a group with nothing in `order`.
pub fn reordered(order: &[DownloadId], moved: &[DownloadId], place: QueuePlace) -> Vec<DownloadId> {
    let unchanged = || order.to_vec();
    let (group, mut rest): (Vec<DownloadId>, Vec<DownloadId>) =
        order.iter().partition(|id| moved.contains(id));
    if group.is_empty() {
        return unchanged();
    }
    let index = match place {
        QueuePlace::End => rest.len(),
        QueuePlace::Before(target) => {
            let Some(at) = order.iter().position(|id| *id == target) else {
                return unchanged();
            };
            // The first download from the target on that is not moving.
            match order[at..].iter().find(|id| !moved.contains(id)) {
                Some(anchor) => rest
                    .iter()
                    .position(|id| id == anchor)
                    .expect("a download that stays put is in the rest"),
                None => rest.len(),
            }
        }
    };
    rest.splice(index..index, group);
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[u64]) -> Vec<DownloadId> {
        values.iter().copied().map(DownloadId).collect()
    }

    fn before(id: u64) -> QueuePlace {
        QueuePlace::Before(DownloadId(id))
    }

    #[test]
    fn moves_before_another_download() {
        let order = ids(&[1, 2, 3, 4]);
        assert_eq!(reordered(&order, &ids(&[4]), before(1)), ids(&[4, 1, 2, 3]));
        assert_eq!(reordered(&order, &ids(&[1]), before(3)), ids(&[2, 1, 3, 4]));
        assert_eq!(reordered(&order, &ids(&[2]), before(3)), ids(&[1, 2, 3, 4]));
    }

    #[test]
    fn moves_to_the_end() {
        let order = ids(&[1, 2, 3]);
        assert_eq!(
            reordered(&order, &ids(&[1]), QueuePlace::End),
            ids(&[2, 3, 1])
        );
        assert_eq!(
            reordered(&order, &ids(&[3]), QueuePlace::End),
            ids(&[1, 2, 3])
        );
    }

    #[test]
    fn a_group_moves_together_in_its_start_order() {
        let order = ids(&[1, 2, 3, 4, 5]);
        // Picked out of order, the group still keeps the order it had.
        assert_eq!(
            reordered(&order, &ids(&[4, 2]), before(1)),
            ids(&[2, 4, 1, 3, 5])
        );
        assert_eq!(
            reordered(&order, &ids(&[1, 3]), QueuePlace::End),
            ids(&[2, 4, 5, 1, 3])
        );
        assert_eq!(
            reordered(&order, &ids(&[1, 2]), before(5)),
            ids(&[3, 4, 1, 2, 5])
        );
    }

    #[test]
    fn a_place_inside_the_group_means_after_it() {
        let order = ids(&[1, 2, 3, 4]);
        // Dropped just below itself: nothing moves.
        assert_eq!(reordered(&order, &ids(&[2]), before(2)), order);
        assert_eq!(reordered(&order, &ids(&[2, 3]), before(3)), order);
        // The download after the target is part of the group too.
        assert_eq!(
            reordered(&order, &ids(&[1, 3, 4]), before(3)),
            ids(&[2, 1, 3, 4])
        );
    }

    #[test]
    fn nonsense_places_change_nothing() {
        let order = ids(&[1, 2, 3]);
        assert_eq!(reordered(&order, &ids(&[2]), before(9)), order);
        assert_eq!(reordered(&order, &ids(&[9]), QueuePlace::End), order);
        assert_eq!(reordered(&order, &[], QueuePlace::End), order);
    }
}

//! Which downloads are picked in the list. A plain click picks one,
//! Shift+click picks the run from the last plain or Ctrl click, and
//! Ctrl+click adds or drops one, as in a file browser.

use std::collections::HashSet;

use downpour_core::{DownloadId, DownloadView};

/// How a click changes the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Only this download.
    Only,
    /// Every download from the anchor to this one, in list order (Shift).
    Range,
    /// Add this download, or drop it if it was picked (Ctrl).
    Toggle,
}

impl Pick {
    /// The pick a click with these keys held makes. Shift wins over Ctrl.
    pub fn from_modifiers(modifiers: eframe::egui::Modifiers) -> Pick {
        if modifiers.shift {
            Pick::Range
        } else if modifiers.command {
            Pick::Toggle
        } else {
            Pick::Only
        }
    }
}

/// The picked downloads. Picks can include downloads the current view
/// hides; [`Selection::shown`] says what the picks are in the list as shown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    picked: HashSet<DownloadId>,
    /// Where a Shift range starts: the last plain or Ctrl click.
    anchor: Option<DownloadId>,
    /// The last download clicked, which the arrow keys step from.
    focus: Option<DownloadId>,
}

/// The picked downloads within the list as shown.
#[derive(Debug)]
pub enum Shown<'a> {
    Nothing,
    One(&'a DownloadView),
    /// Two or more, in list order.
    Group(Vec<&'a DownloadView>),
}

impl<'a> Shown<'a> {
    /// The download when exactly one is shown picked.
    pub fn one(&self) -> Option<&'a DownloadView> {
        match self {
            Shown::One(download) => Some(download),
            Shown::Nothing | Shown::Group(_) => None,
        }
    }

    /// The shown picked downloads' ids, in list order.
    pub fn ids(&self) -> Vec<DownloadId> {
        match self {
            Shown::Nothing => Vec::new(),
            Shown::One(download) => vec![download.id],
            Shown::Group(group) => group.iter().map(|download| download.id).collect(),
        }
    }
}

impl Selection {
    pub fn only(id: DownloadId) -> Selection {
        Selection {
            picked: HashSet::from([id]),
            anchor: Some(id),
            focus: Some(id),
        }
    }

    pub fn clear(&mut self) {
        *self = Selection::default();
    }

    /// Applies a click on `id`; `visible` is the list as shown, which a
    /// Shift range runs through.
    pub fn pick(&mut self, id: DownloadId, pick: Pick, visible: &[DownloadId]) {
        match pick {
            Pick::Only => *self = Selection::only(id),
            Pick::Toggle => {
                if !self.picked.remove(&id) {
                    self.picked.insert(id);
                }
                self.anchor = Some(id);
                self.focus = Some(id);
            }
            Pick::Range => {
                let from = self
                    .anchor
                    .and_then(|anchor| visible.iter().position(|shown| *shown == anchor));
                let to = visible.iter().position(|shown| *shown == id);
                // With no anchor in view there is no run to pick: start one here.
                let (Some(from), Some(to)) = (from, to) else {
                    *self = Selection::only(id);
                    return;
                };
                let run = if from <= to { from..=to } else { to..=from };
                // The anchor stays, so the next Shift+click redraws the run.
                self.picked = visible[run].iter().copied().collect();
                self.focus = Some(id);
            }
        }
    }

    pub fn contains(&self, id: DownloadId) -> bool {
        self.picked.contains(&id)
    }

    pub fn is_empty(&self) -> bool {
        self.picked.is_empty()
    }

    /// The download the arrow keys step from: the last one clicked.
    pub fn focus(&self) -> Option<DownloadId> {
        self.focus.filter(|focus| self.contains(*focus))
    }

    /// What the picks are in `visible`, the list as shown. Every question
    /// about "the selected download" goes through here, so hidden picks
    /// count nowhere.
    pub fn shown<'a>(&self, visible: &[&'a DownloadView]) -> Shown<'a> {
        let mut group: Vec<&'a DownloadView> = visible
            .iter()
            .copied()
            .filter(|download| self.contains(download.id))
            .collect();
        match group.len() {
            0 => Shown::Nothing,
            1 => Shown::One(group.remove(0)),
            _ => Shown::Group(group),
        }
    }

    /// Drops picked downloads that no longer exist.
    pub fn retain(&mut self, exists: impl Fn(DownloadId) -> bool) {
        self.picked.retain(|id| exists(*id));
        if self.anchor.is_some_and(|anchor| !exists(anchor)) {
            self.anchor = None;
        }
        if self.focus.is_some_and(|focus| !exists(focus)) {
            self.focus = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::view;
    use super::*;

    fn ids(values: &[u64]) -> Vec<DownloadId> {
        values.iter().copied().map(DownloadId).collect()
    }

    fn picked(selection: &Selection) -> Vec<u64> {
        let mut picked: Vec<u64> = selection.picked.iter().map(|id| id.0).collect();
        picked.sort_unstable();
        picked
    }

    #[test]
    fn a_plain_click_picks_only_that_download() {
        let list = ids(&[1, 2, 3]);
        let mut selection = Selection::only(DownloadId(1));
        selection.pick(DownloadId(3), Pick::Toggle, &list);
        selection.pick(DownloadId(2), Pick::Only, &list);
        assert_eq!(picked(&selection), [2]);
    }

    #[test]
    fn shift_picks_the_run_from_the_last_click_either_way() {
        let list = ids(&[1, 2, 3, 4, 5]);
        let mut selection = Selection::only(DownloadId(2));
        selection.pick(DownloadId(4), Pick::Range, &list);
        assert_eq!(picked(&selection), [2, 3, 4]);
        // The anchor stays: a second Shift+click redraws the run from it.
        selection.pick(DownloadId(1), Pick::Range, &list);
        assert_eq!(picked(&selection), [1, 2]);
    }

    #[test]
    fn the_arrow_keys_step_from_the_end_of_a_shift_run() {
        let list = ids(&[1, 2, 3, 4, 5]);
        let mut selection = Selection::only(DownloadId(2));
        selection.pick(DownloadId(4), Pick::Range, &list);
        assert_eq!(selection.focus(), Some(DownloadId(4)));
    }

    #[test]
    fn shift_with_nothing_picked_starts_at_the_click() {
        let list = ids(&[1, 2, 3]);
        let mut selection = Selection::default();
        selection.pick(DownloadId(2), Pick::Range, &list);
        assert_eq!(picked(&selection), [2]);
        selection.pick(DownloadId(3), Pick::Range, &list);
        assert_eq!(picked(&selection), [2, 3]);
    }

    #[test]
    fn ctrl_adds_and_drops_one_download() {
        let list = ids(&[1, 2, 3]);
        let mut selection = Selection::only(DownloadId(1));
        selection.pick(DownloadId(3), Pick::Toggle, &list);
        assert_eq!(picked(&selection), [1, 3]);
        selection.pick(DownloadId(1), Pick::Toggle, &list);
        assert_eq!(picked(&selection), [3]);
    }

    #[test]
    fn only_the_picks_the_list_shows_count() {
        let (one, two, three) = (view(1), view(2), view(3));
        let mut selection = Selection::only(DownloadId(3));
        selection.pick(DownloadId(1), Pick::Toggle, &ids(&[1, 2, 3]));
        // Both shown: a group, in list order.
        assert_eq!(selection.shown(&[&one, &two, &three]).ids(), ids(&[1, 3]));
        // Another view hides one: the other is the single selected download.
        let one_shown = selection.shown(&[&two, &three]).one();
        assert_eq!(one_shown.map(|download| download.id), Some(DownloadId(3)));
        assert!(matches!(selection.shown(&[&two]), Shown::Nothing));
    }

    #[test]
    fn removed_downloads_leave_the_selection() {
        let list = ids(&[1, 2, 3]);
        let mut selection = Selection::only(DownloadId(1));
        selection.pick(DownloadId(3), Pick::Range, &list);
        selection.retain(|id| id != DownloadId(1));
        assert_eq!(picked(&selection), [2, 3]);
        assert_eq!(selection.focus(), Some(DownloadId(3)));
        selection.retain(|id| id != DownloadId(3));
        assert_eq!(selection.focus(), None);
    }
}

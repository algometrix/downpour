use std::cmp::Ordering;
use std::num::NonZeroU32;
use std::time::{SystemTime, UNIX_EPOCH};

use downpour_core::limiter::DownloadLimit;
use downpour_core::{
    CategoryLook, DownloadId, DownloadStatus, DownloadView, Queue, QueueId, QueuePlace, QueueState,
};
use eframe::egui::{
    self, pos2, vec2, Align, Color32, CornerRadius, Layout, Painter, Rect, Sense, Ui,
};

use super::icons::Icon;
use super::selection::{Pick, Selection, Shown};
use super::theme::RowStyle;
use super::widgets::{
    self, button, button_width, dropdown, elided, galley, icon_button, icon_button_at, pill,
    progress_bar, solid_tile_colors, stat_tile_row, ButtonKind, StatTile, TileColors,
};
use super::{download_page, format, hotkeys, queue_state_label, theme, Action, Catalog, Filter};
use crate::app_settings::{ListOrder, SortColumn, SortDirection};

const ROW_HEIGHT: f32 = 64.0;
const COLUMN_GAP: f32 = 16.0;
const TILE: f32 = 40.0;
const ICON_BUTTON: f32 = 32.0;
const ACTIONS_WIDTH: f32 = 3.0 * ICON_BUTTON + 2.0 * 4.0;
const STATUS_WIDTH: f32 = 112.0;
const SPEED_WIDTH: f32 = 100.0;
const SIZE_WIDTH: f32 = 84.0;
const PROGRESS_WIDTH: f32 = 180.0;
const MIN_NAME_WIDTH: f32 = 150.0;
const SEARCH_MIN_WIDTH: f32 = 180.0;
const SEARCH_MAX_WIDTH: f32 = 380.0;
/// The search field's width with labeled bulk buttons beside it: room for its
/// whole hint, "Search downloads… (Ctrl+F)". Narrower, the buttons give way.
const SEARCH_LABELED_MIN_WIDTH: f32 = 260.0;
/// Space kept between the search side of the toolbar and its buttons.
const TOOLBAR_GAP: f32 = 24.0;
/// Room between the list card's edge and its rows.
const LIST_CARD_MARGIN: egui::Margin = egui::Margin::symmetric(12, 14);
/// Space between two row cards; half of it sits above a card and half
/// below, inside the row's hit target.
const ROW_CARD_GAP: f32 = 12.0;
/// Room between a row card's edge and its content.
const ROW_CARD_PADDING: f32 = 8.0;
/// How strongly a row in a card or band leans toward the primary color.
const SELECTED_ROW_TINT: f32 = 0.18;
const HOVERED_ROW_TINT: f32 = 0.08;
/// How much tighter a row's highlight corners are than the card around it.
const ROW_CORNER_TIGHTENING: u8 = 4;

pub struct ListState<'a> {
    pub downloads: &'a [DownloadView],
    pub catalog: &'a Catalog<'a>,
    pub filter: Filter,
    pub search: &'a mut String,
    pub selection: &'a Selection,
    pub order: ListOrder,
    /// The downloads shown for this view and search, in list order; the
    /// app works them out once a frame.
    pub visible: &'a [&'a DownloadView],
}

pub fn show(ui: &mut Ui, state: ListState<'_>, actions: &mut Vec<Action>) {
    toolbar(
        ui,
        state.search,
        sorting_for(state.filter, state.order),
        actions,
    );
    ui.add_space(18.0);

    let visible = state.visible;
    let completed = visible
        .iter()
        .filter(|download| download.status == DownloadStatus::Completed)
        .count();
    heading(
        ui,
        state.catalog,
        state.filter,
        ViewCounts {
            shown: visible.len(),
            completed,
        },
        actions,
    );
    ui.add_space(14.0);
    summary_tiles(ui, state.downloads);
    ui.add_space(18.0);

    let style = theme::form().rows;
    match style {
        // Each row is a card of its own, so the list sits on the page.
        RowStyle::Cards => download_rows(ui, &state, visible, style, actions),
        RowStyle::Lines | RowStyle::Bands => {
            theme::card(ui, LIST_CARD_MARGIN, |ui| {
                ui.set_min_size(ui.available_size().max(egui::Vec2::ZERO));
                download_rows(ui, &state, visible, style, actions);
            });
        }
    }
}

/// The column header and the scrolling rows, or the empty state.
fn download_rows<'a>(
    ui: &mut Ui,
    state: &ListState<'_>,
    visible: &'a [&'a DownloadView],
    style: RowStyle,
    actions: &mut Vec<Action>,
) {
    if visible.is_empty() {
        empty_state(ui, state.filter, !state.search.is_empty(), actions);
        return;
    }
    let geometry = RowGeometry::for_style(style);
    let columns = Columns::for_width(geometry.content_width(ui.available_width()));
    let sorting = sorting_for(state.filter, state.order);
    header(ui, &columns, style, geometry, sorting, actions);
    let shown = state.selection.shown(visible);
    let rows = RowList {
        catalog: state.catalog,
        columns: &columns,
        style,
        geometry,
        visible,
        selection: state.selection,
        shown: &shown,
    };
    // A queue view shows the start order, so its rows can be dragged and
    // moved; other views are filtered lists.
    let in_queue = matches!(sorting, Sorting::StartOrderOnly);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(geometry.edge_room());
            let first = visible.first().map(|download| download.id);
            for (index, download) in visible.iter().enumerate() {
                let reorder = match first {
                    Some(first) if in_queue => Reorder::InQueue {
                        first,
                        next: visible.get(index + 1).map(|next| next.id),
                    },
                    _ => Reorder::Off,
                };
                row(ui, &rows, index, download, reorder, actions);
            }
            ui.add_space(geometry.edge_room());
        });
    dragged_badge(ui.ctx(), in_queue);
}

/// What a dragged row carries: the downloads it moves, in list order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraggedDownloads(pub Vec<DownloadId>);

/// While rows are dragged, a badge at the pointer says how many and where
/// they can go: in a queue view, rows also take a drop to reorder.
fn dragged_badge(ctx: &egui::Context, in_queue: bool) {
    let Some(dragged) = egui::DragAndDrop::payload::<DraggedDownloads>(ctx) else {
        return;
    };
    let Some(pointer) = ctx.pointer_interact_pos() else {
        return;
    };
    let count = match dragged.0.len() {
        1 => "1 download".to_owned(),
        many => format!("{many} downloads"),
    };
    let hint = if in_queue {
        "Drop between rows to reorder, or on a queue to move"
    } else {
        "Drop on a queue to move"
    };
    egui::Area::new(egui::Id::new("dragged-downloads"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pointer + vec2(16.0, 12.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(
                    egui::RichText::new(count)
                        .font(theme::semibold(13.0))
                        .color(theme::ink()),
                );
                ui.label(
                    egui::RichText::new(hint)
                        .font(theme::regular(12.0))
                        .color(theme::ink_soft()),
                );
            });
        });
}

/// The downloads the list shows for this view and search, in list order.
pub fn visible_downloads<'a>(
    downloads: &'a [DownloadView],
    filter: Filter,
    search: &str,
    order: ListOrder,
) -> Vec<&'a DownloadView> {
    let mut visible: Vec<&DownloadView> = downloads
        .iter()
        .filter(|download| filter.includes(download) && matches_search(download, search))
        .collect();
    if let Sorting::Sortable(order) = sorting_for(filter, order) {
        sort_downloads(&mut visible, order);
    }
    visible
}

/// Whether this view can be sorted. A queue view shows the start order,
/// which its rows are dragged to change.
fn sorting_for(filter: Filter, order: ListOrder) -> Sorting {
    match filter {
        Filter::Queue(_) => Sorting::StartOrderOnly,
        _ => Sorting::Sortable(order),
    }
}

/// Sorts by the chosen column; downloads that compare equal keep their
/// start order, and values not known yet (a size the server never sent)
/// go last in either direction.
fn sort_downloads(downloads: &mut [&DownloadView], order: ListOrder) {
    let ListOrder::Column { column, direction } = order else {
        return;
    };
    downloads.sort_by(|a, b| {
        let unknown = is_unknown(a, column).cmp(&is_unknown(b, column));
        unknown.then_with(|| {
            let ascending = compare_ascending(a, b, column);
            match direction {
                SortDirection::Ascending => ascending,
                SortDirection::Descending => ascending.reverse(),
            }
        })
    });
}

fn is_unknown(download: &DownloadView, column: SortColumn) -> bool {
    match column {
        SortColumn::Size => download.size.is_none(),
        SortColumn::Progress => download.progress_fraction.is_none(),
        SortColumn::Finished => download.finished_at_unix.is_none(),
        SortColumn::Name | SortColumn::Speed | SortColumn::Status | SortColumn::Added => false,
    }
}

fn compare_ascending(a: &DownloadView, b: &DownloadView, column: SortColumn) -> Ordering {
    match column {
        // Compared letter by letter: the list sorts on every frame.
        SortColumn::Name => a
            .file_name
            .chars()
            .flat_map(char::to_lowercase)
            .cmp(b.file_name.chars().flat_map(char::to_lowercase)),
        SortColumn::Size => a.size.cmp(&b.size),
        SortColumn::Progress => {
            let fraction = |download: &DownloadView| download.progress_fraction.unwrap_or(0.0);
            fraction(a).total_cmp(&fraction(b))
        }
        SortColumn::Speed => a.bytes_per_second.cmp(&b.bytes_per_second),
        SortColumn::Status => activity_rank(&a.status).cmp(&activity_rank(&b.status)),
        SortColumn::Added => a.added_at_unix.cmp(&b.added_at_unix),
        SortColumn::Finished => a.finished_at_unix.cmp(&b.finished_at_unix),
    }
}

/// Status sorts by how much a download is doing: transferring first,
/// finished last.
fn activity_rank(status: &DownloadStatus) -> u8 {
    match status {
        DownloadStatus::Downloading => 0,
        DownloadStatus::Connecting => 1,
        DownloadStatus::Queued => 2,
        DownloadStatus::WaitingForSpace { .. } => 3,
        DownloadStatus::Paused => 4,
        DownloadStatus::Failed { .. } => 5,
        DownloadStatus::Completed => 6,
    }
}

/// Which way the arrow keys move the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Next,
    Previous,
}

/// The download one step from `current` in the list. With nothing
/// selected (or the selection out of view), Next picks the first and
/// Previous the last; at either end the selection stays put.
pub fn neighbor<'a>(
    visible: &[&'a DownloadView],
    current: Option<DownloadId>,
    step: Step,
) -> Option<&'a DownloadView> {
    let position = current.and_then(|id| visible.iter().position(|download| download.id == id));
    let index = match (position, step) {
        (None, Step::Next) => 0,
        (None, Step::Previous) => visible.len().checked_sub(1)?,
        (Some(at), Step::Next) => (at + 1).min(visible.len() - 1),
        (Some(at), Step::Previous) => at.saturating_sub(1),
    };
    visible.get(index).copied()
}

fn matches_search(download: &DownloadView, search: &str) -> bool {
    let needle = search.trim().to_lowercase();
    needle.is_empty()
        || download.file_name.to_lowercase().contains(&needle)
        || download.url.as_str().to_lowercase().contains(&needle)
}

/// Shown on hover, so the two ways of adding downloads explain themselves.
pub const ADD_URL_HELP: &str =
    "Download one file: paste its address, then choose its name, folder, and queue.";
pub const ADD_LINKS_HELP: &str =
    "Paste many links (or a whole page of text): Downpour checks which \
     files are still online, and you pick the ones to download.";

/// Search and the Sort menu on the left, bulk actions on the right. When the
/// panel is too narrow for labeled buttons, the bulk actions collapse to icon
/// buttons.
fn toolbar(ui: &mut Ui, search: &mut String, sorting: Sorting, actions: &mut Vec<Action>) {
    let add_url = hotkeys::ADD_URL.label("Add URL");
    let add_links = hotkeys::ADD_LINKS.label("Add links");
    let resume_all = hotkeys::RESUME_ALL.label("Resume all");
    let pause_all = hotkeys::PAUSE_ALL.label("Pause all");
    let gap = ui.spacing().item_spacing.x;
    let add_width = button_width(ui, Some(Icon::Plus), &add_url)
        + button_width(ui, Some(Icon::Link), &add_links)
        + gap;
    let labeled_width = add_width
        + button_width(ui, Some(Icon::Play), &resume_all)
        + button_width(ui, Some(Icon::Pause), &pause_all)
        + gap * 2.0;
    let compact_width = add_width + 2.0 * 32.0 + gap * 2.0;
    let sort_width = match sorting {
        Sorting::Sortable(_) => SORT_MENU_WIDTH + gap,
        Sorting::StartOrderOnly => 0.0,
    };
    let available = ui.available_width() - sort_width;
    let labeled = available - labeled_width - TOOLBAR_GAP >= SEARCH_LABELED_MIN_WIDTH;
    let buttons_width = if labeled {
        labeled_width
    } else {
        compact_width
    };
    let search_width = (available - buttons_width - TOOLBAR_GAP)
        .clamp(SEARCH_MIN_WIDTH.min(available), SEARCH_MAX_WIDTH);

    ui.horizontal(|ui| {
        let field = widgets::text_field(
            ui,
            search,
            &hotkeys::SEARCH.label("Search downloads…"),
            Some(Icon::Search),
            search_width,
        );
        if hotkeys::take_search_focus_request(ui.ctx()) {
            field.request_focus();
        }
        if let Sorting::Sortable(order) = sorting {
            sort_menu(ui, order, actions);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if button(ui, ButtonKind::Primary, Some(Icon::Plus), &add_url)
                .on_hover_text(ADD_URL_HELP)
                .clicked()
            {
                actions.push(Action::OpenAddUrl);
            }
            if button(ui, ButtonKind::Secondary, Some(Icon::Link), &add_links)
                .on_hover_text(ADD_LINKS_HELP)
                .clicked()
            {
                actions.push(Action::OpenLinkGrabber);
            }
            if labeled {
                if button(ui, ButtonKind::Secondary, Some(Icon::Play), &resume_all).clicked() {
                    actions.push(Action::ResumeAll);
                }
                if button(ui, ButtonKind::Secondary, Some(Icon::Pause), &pause_all).clicked() {
                    actions.push(Action::PauseAll);
                }
            } else {
                if icon_button(ui, Icon::Play, &resume_all).clicked() {
                    actions.push(Action::ResumeAll);
                }
                if icon_button(ui, Icon::Pause, &pause_all).clicked() {
                    actions.push(Action::PauseAll);
                }
            }
        });
    });
}

const SORT_MENU_WIDTH: f32 = 190.0;

/// Every way the list sorts, including dates, which have no column. Picking
/// the current one turns the order around.
fn sort_menu(ui: &mut Ui, order: ListOrder, actions: &mut Vec<Action>) {
    let current = match order {
        ListOrder::StartOrder => "Sort: Start order".to_owned(),
        ListOrder::Column { column, direction } => {
            format!("Sort: {}{}", column.label(), direction_arrow(direction))
        }
    };
    widgets::dropdown(ui, "list-sort", SORT_MENU_WIDTH, &current, |ui| {
        let start_order = order == ListOrder::StartOrder;
        if ui.selectable_label(start_order, "Start order").clicked() {
            actions.push(Action::SetListOrder(ListOrder::StartOrder));
        }
        for column in SortColumn::ALL {
            let label = match order {
                ListOrder::Column {
                    column: sorted,
                    direction,
                } if sorted == column => {
                    format!("{}{}", column.label(), direction_arrow(direction))
                }
                _ => column.label().to_owned(),
            };
            let chosen =
                matches!(order, ListOrder::Column { column: sorted, .. } if sorted == column);
            if ui.selectable_label(chosen, label).clicked() {
                actions.push(Action::SetListOrder(order.after_choosing(column)));
            }
        }
    })
    .on_hover_text("Sort the list. Pick the current choice again to reverse it.");
}

fn direction_arrow(direction: SortDirection) -> &'static str {
    match direction {
        SortDirection::Ascending => " ↑",
        SortDirection::Descending => " ↓",
    }
}

/// How many downloads the view shows, and how many of those are finished.
#[derive(Clone, Copy)]
struct ViewCounts {
    shown: usize,
    completed: usize,
}

/// The view's title; a queue also shows its state, its limit, and Start/Stop.
/// Views with finished downloads offer to clear them from the list.
fn heading(
    ui: &mut Ui,
    catalog: &Catalog<'_>,
    filter: Filter,
    counts: ViewCounts,
    actions: &mut Vec<Action>,
) {
    let count = counts.shown;
    let queue = match filter {
        Filter::Queue(id) => catalog.queue(id),
        _ => None,
    };
    let title = catalog.title(filter);
    // Light and Display titles are larger: they shrink, then are cut
    // short, before they would run under the controls beside them.
    let title_width =
        (ui.available_width() - heading_controls_width(ui, queue, counts) - COLUMN_GAP).max(0.0);
    let title_font = widgets::fitted_font(
        ui,
        &title,
        theme::title,
        TITLE_SIZE,
        TITLE_MIN_SIZE,
        title_width,
    );
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_max_width(title_width);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(title)
                        .font(title_font)
                        .color(theme::ink()),
                )
                .truncate(),
            );
            let noun = if count == 1 { "item" } else { "items" };
            let subtitle = match queue {
                Some(queue) => format!(
                    "{count} {noun} · {} · up to {} at a time",
                    queue_state_label(queue),
                    queue.max_active
                ),
                None => format!("{count} {noun}"),
            };
            ui.add(
                egui::Label::new(
                    egui::RichText::new(subtitle)
                        .font(theme::regular(13.0))
                        .color(theme::ink_soft()),
                )
                .truncate(),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(queue) = queue {
                queue_controls(ui, queue, actions);
            }
            if counts.completed > 0
                && button(
                    ui,
                    ButtonKind::Secondary,
                    Some(Icon::Check),
                    &clear_completed_label(),
                )
                .on_hover_text(CLEAR_COMPLETED_HELP)
                .clicked()
            {
                actions.push(Action::ClearCompleted);
            }
        });
    });
}

/// The page title's size in the standard style; see [`theme::title`].
const TITLE_SIZE: f32 = 26.0;
/// The smallest a page title shrinks to before it is cut short.
const TITLE_MIN_SIZE: f32 = 18.0;

/// The width the heading's buttons take at its right, as [`heading`]
/// lays them out.
fn heading_controls_width(ui: &Ui, queue: Option<&Queue>, counts: ViewCounts) -> f32 {
    let mut widths = Vec::new();
    if let Some(queue) = queue {
        let toggle = QueueToggle::for_queue(queue);
        widths.extend([
            button_width(ui, Some(toggle.icon), &toggle.label),
            QUEUE_LIMIT_WIDTH,
            ICON_BUTTON,
        ]);
    }
    if counts.completed > 0 {
        widths.push(button_width(
            ui,
            Some(Icon::Check),
            &clear_completed_label(),
        ));
    }
    let gaps = ui.spacing().item_spacing.x * widths.len() as f32;
    widths.iter().sum::<f32>() + gaps
}

fn clear_completed_label() -> String {
    hotkeys::CLEAR_COMPLETED.label("Clear completed")
}

pub const CLEAR_COMPLETED_HELP: &str = "Removes finished downloads from the list. \
     The files stay on disk, and Statistics still counts them.";

/// The button that starts a stopped queue or stops a running one.
struct QueueToggle {
    kind: ButtonKind,
    icon: Icon,
    label: String,
    action: Action,
}

impl QueueToggle {
    fn for_queue(queue: &Queue) -> QueueToggle {
        let (kind, icon, label, action) = match queue.state {
            QueueState::Running => (
                ButtonKind::Secondary,
                Icon::Pause,
                "Stop queue",
                Action::StopQueue(queue.id),
            ),
            QueueState::Stopped => (
                ButtonKind::Primary,
                Icon::Play,
                "Start queue",
                Action::StartQueue(queue.id),
            ),
        };
        QueueToggle {
            kind,
            icon,
            label: hotkeys::START_STOP_QUEUE.label(label),
            action,
        }
    }
}

fn queue_controls(ui: &mut Ui, queue: &Queue, actions: &mut Vec<Action>) {
    let toggle = QueueToggle::for_queue(queue);
    if button(ui, toggle.kind, Some(toggle.icon), &toggle.label).clicked() {
        actions.push(toggle.action);
    }
    let mut limit = queue.max_active;
    dropdown(
        ui,
        ("queue-limit", queue.id),
        QUEUE_LIMIT_WIDTH,
        &format!("{limit} at a time"),
        |ui| {
            for count in downpour_core::queue::QUEUE_LIMIT_RANGE {
                ui.selectable_value(&mut limit, count, format!("{count} at a time"));
            }
        },
    )
    .on_hover_text("How many downloads from this queue run at the same time");
    if limit != queue.max_active {
        actions.push(Action::SetQueueLimit(queue.id, limit));
    }
    if icon_button(ui, Icon::Gear, "Rename or change this queue").clicked() {
        actions.push(Action::RenameQueue(queue.id));
    }
}

const QUEUE_LIMIT_WIDTH: f32 = 132.0;

fn summary_tiles(ui: &mut Ui, downloads: &[DownloadView]) {
    let speed: u64 = downloads
        .iter()
        .map(|download| download.bytes_per_second)
        .sum();
    let count = |wanted: fn(&DownloadStatus) -> bool| {
        downloads
            .iter()
            .filter(|d| wanted(&d.status))
            .count()
            .to_string()
    };
    let active = count(DownloadStatus::is_active);
    let queued = count(|status| *status == DownloadStatus::Queued);
    let completed = count(|status| *status == DownloadStatus::Completed);
    let speed = format::speed(speed);
    let [speed_colors, active_colors, queued_colors, completed_colors] = summary_colors();
    let tiles = [
        StatTile {
            value: &speed,
            label: "Total speed",
            icon: Icon::Bolt,
            colors: speed_colors,
        },
        StatTile {
            value: &active,
            label: "Downloading",
            icon: Icon::Download,
            colors: active_colors,
        },
        StatTile {
            value: &queued,
            label: "Queued",
            icon: Icon::Clock,
            colors: queued_colors,
        },
        StatTile {
            value: &completed,
            label: "Completed",
            icon: Icon::Check,
            colors: completed_colors,
        },
    ];

    stat_tile_row(ui, &tiles);
}

/// The colors of the four summary tiles, left to right.
fn summary_colors() -> [TileColors; 4] {
    match theme::form().tiles {
        theme::TileStyle::Solid => solid_tile_colors(),
        theme::TileStyle::Tinted | theme::TileStyle::Neutral => [
            TileColors::new(theme::success(), theme::tile_mint()),
            TileColors::new(theme::primary(), theme::primary_soft()),
            TileColors::new(theme::accent(), theme::tile_peach()),
            TileColors::new(
                theme::look_colors(CategoryLook::Music).0,
                theme::tile_pink(),
            ),
        ],
    }
}

/// Column layout for the current width. As space runs out, columns give way
/// in order of importance, so text is elided rather than overlapped:
/// size, then speed, then the progress column narrows, then progress folds
/// into the name cell, and finally only the primary action button remains.
struct Columns {
    name: f32,
    size: Option<f32>,
    /// `None` shows progress inside the name cell instead of its own column.
    progress: Option<f32>,
    speed: Option<f32>,
    actions: RowActions,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowActions {
    /// Pause/resume, show in folder, remove.
    All,
    /// Pause/resume only; the rest stays reachable from the details panel.
    PrimaryOnly,
}

impl RowActions {
    fn width(self) -> f32 {
        match self {
            RowActions::All => ACTIONS_WIDTH,
            RowActions::PrimaryOnly => 32.0,
        }
    }
}

const MIN_PROGRESS_WIDTH: f32 = 110.0;
const NAME_LEFT: f32 = 8.0 + TILE + 12.0;

impl Columns {
    fn for_width(width: f32) -> Columns {
        let mut columns = Columns {
            name: 0.0,
            size: Some(SIZE_WIDTH),
            progress: Some(PROGRESS_WIDTH),
            speed: Some(SPEED_WIDTH),
            actions: RowActions::All,
        };
        if columns.name_width(width) < MIN_NAME_WIDTH {
            columns.size = None;
        }
        if columns.name_width(width) < MIN_NAME_WIDTH {
            columns.speed = None;
        }
        if columns.name_width(width) < MIN_NAME_WIDTH {
            let shortfall = MIN_NAME_WIDTH - columns.name_width(width);
            columns.progress = Some((PROGRESS_WIDTH - shortfall).max(MIN_PROGRESS_WIDTH));
        }
        if columns.name_width(width) < MIN_NAME_WIDTH {
            columns.progress = None;
        }
        if columns.name_width(width) < MIN_NAME_WIDTH {
            columns.actions = RowActions::PrimaryOnly;
        }
        columns.name = columns.name_width(width).max(40.0);
        columns
    }

    fn name_width(&self, width: f32) -> f32 {
        let optional = |column: Option<f32>| column.map_or(0.0, |w| w + COLUMN_GAP);
        width
            - NAME_LEFT
            - COLUMN_GAP
            - optional(self.size)
            - optional(self.progress)
            - optional(self.speed)
            - STATUS_WIDTH
            - COLUMN_GAP
            - self.actions.width()
            - 8.0
    }

    /// Left x of each column, relative to the row's left edge.
    fn offsets(&self) -> ColumnOffsets {
        let mut x = NAME_LEFT + self.name + COLUMN_GAP;
        let mut place = |column: Option<f32>| {
            column.map(|w| {
                let at = x;
                x += w + COLUMN_GAP;
                at
            })
        };
        let size = place(self.size);
        let progress = place(self.progress);
        let speed = place(self.speed);
        ColumnOffsets {
            name: NAME_LEFT,
            size,
            progress,
            speed,
            status: x,
        }
    }
}

struct ColumnOffsets {
    name: f32,
    size: Option<f32>,
    progress: Option<f32>,
    speed: Option<f32>,
    status: f32,
}

/// Whether the column headings sort the list.
#[derive(Clone, Copy)]
enum Sorting {
    Sortable(ListOrder),
    /// A queue view: rows show, and are dragged to change, the start order.
    StartOrderOnly,
}

fn header(
    ui: &mut Ui,
    columns: &Columns,
    style: RowStyle,
    geometry: RowGeometry,
    sorting: Sorting,
    actions: &mut Vec<Action>,
) {
    let (slot, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    // Column names line up with the row content, inside any row card.
    let rect = slot.shrink2(vec2(geometry.padding, 0.0));
    let offsets = columns.offsets();
    let optional = |x: Option<f32>, width: Option<f32>, title, column| {
        x.zip(width).map(|(x, width)| (x, width, title, column))
    };
    let headings = [
        Some((offsets.name, columns.name, "NAME", SortColumn::Name)),
        optional(offsets.size, columns.size, "SIZE", SortColumn::Size),
        optional(
            offsets.progress,
            columns.progress,
            "PROGRESS",
            SortColumn::Progress,
        ),
        optional(offsets.speed, columns.speed, "SPEED", SortColumn::Speed),
        Some((offsets.status, STATUS_WIDTH, "STATUS", SortColumn::Status)),
    ];
    for (x, width, title, column) in headings.into_iter().flatten() {
        let area = Rect::from_min_size(
            pos2(rect.left() + x, rect.top()),
            vec2(width, rect.height()),
        );
        column_heading(ui, area, title, column, sorting, actions);
    }
    match style {
        RowStyle::Lines => {
            ui.painter().line_segment(
                [
                    pos2(rect.left(), rect.bottom()),
                    pos2(rect.right(), rect.bottom()),
                ],
                egui::Stroke::new(1.0, theme::border()),
            );
        }
        // Cards and bands part the rows by shape and shade, not lines.
        RowStyle::Cards | RowStyle::Bands => {}
    }
    ui.add_space(4.0);
}

/// One column's title; where the list can be sorted, a click sorts by it and
/// an arrow marks the column it is sorted by.
fn column_heading(
    ui: &mut Ui,
    area: Rect,
    title: &str,
    column: SortColumn,
    sorting: Sorting,
    actions: &mut Vec<Action>,
) {
    let id = ui.id().with(("sort-heading", title));
    let (text, color) = match sorting {
        Sorting::StartOrderOnly => {
            ui.interact(area, id, Sense::hover()).on_hover_text(
                "A queue lists its start order. Sort in All downloads or another view.",
            );
            (title.to_owned(), theme::ink_faint())
        }
        Sorting::Sortable(order) => {
            let response = ui
                .interact(area, id, Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                actions.push(Action::SortBy(column));
            }
            let arrow = match order {
                ListOrder::Column {
                    column: sorted,
                    direction,
                } if sorted == column => direction_arrow(direction),
                _ => "",
            };
            let color = if response.hovered() || !arrow.is_empty() {
                theme::ink_soft()
            } else {
                theme::ink_faint()
            };
            response.on_hover_text(format!("Sort by {}", title.to_lowercase()));
            (format!("{title}{arrow}"), color)
        }
    };
    let text = galley(ui, text, theme::semibold(11.5), color);
    ui.painter().galley(
        pos2(area.left(), area.center().y - text.size().y / 2.0),
        text,
        color,
    );
}

/// Where a row's parts sit inside the slot it takes in the list.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RowGeometry {
    /// Space between two rows' bodies. Half of it sits above a body and
    /// half below, inside the row's hit target, so a drag never falls
    /// between rows.
    gap: f32,
    /// Room between a row body's edge and its content.
    padding: f32,
}

impl RowGeometry {
    fn for_style(style: RowStyle) -> RowGeometry {
        match style {
            RowStyle::Lines | RowStyle::Bands => RowGeometry {
                gap: 0.0,
                padding: 0.0,
            },
            RowStyle::Cards => RowGeometry {
                gap: ROW_CARD_GAP,
                padding: ROW_CARD_PADDING,
            },
        }
    }

    fn slot_height(self) -> f32 {
        ROW_HEIGHT + self.gap
    }

    /// The part of `slot` that is painted: the highlight, card, or band.
    fn body(self, slot: Rect) -> Rect {
        slot.shrink2(vec2(0.0, self.gap / 2.0))
    }

    /// Where the columns are laid out, `ROW_HEIGHT` tall.
    fn content(self, slot: Rect) -> Rect {
        self.body(slot).shrink2(vec2(self.padding, 0.0))
    }

    /// The width the columns get in a list `width` wide.
    fn content_width(self, width: f32) -> f32 {
        width - 2.0 * self.padding
    }

    /// Space before the first row and after the last: with the half gap
    /// inside each slot, the end cards keep a full gap for their shadows,
    /// which the scroll area would otherwise cut.
    fn edge_room(self) -> f32 {
        self.gap / 2.0
    }
}

/// What every row of one list shares.
struct RowList<'a> {
    catalog: &'a Catalog<'a>,
    columns: &'a Columns,
    style: RowStyle,
    geometry: RowGeometry,
    /// The rows as shown, which a drag or a group menu acts on in order.
    visible: &'a [&'a DownloadView],
    selection: &'a Selection,
    /// The picks among `visible`.
    shown: &'a Shown<'a>,
}

impl<'a> RowList<'a> {
    /// The downloads a drag or right-click on `download` acts on: the shown
    /// group when it is one of them, otherwise it alone.
    fn group_of(&self, download: &'a DownloadView) -> Vec<&'a DownloadView> {
        match self.shown {
            Shown::Group(group) if group.iter().any(|picked| picked.id == download.id) => {
                group.clone()
            }
            Shown::Nothing | Shown::One(_) | Shown::Group(_) => vec![download],
        }
    }
}

/// How strongly a row stands out from its neighbors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowEmphasis {
    Selected,
    Hovered,
    Resting,
}

/// The two shades banded rows alternate between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BandShade {
    Soft,
    Surface,
}

/// The shade of the `index`th band; the first row starts soft.
fn band_shade(index: usize) -> BandShade {
    if index % 2 == 0 {
        BandShade::Soft
    } else {
        BandShade::Surface
    }
}

/// Paints behind a row's content in the theme's row style.
fn paint_row_body(
    painter: &Painter,
    style: RowStyle,
    body: Rect,
    emphasis: RowEmphasis,
    shade: BandShade,
) {
    match style {
        RowStyle::Lines => {
            let fill = match emphasis {
                RowEmphasis::Selected => theme::selected_row(),
                RowEmphasis::Hovered => theme::surface_muted(),
                RowEmphasis::Resting => return,
            };
            painter.rect_filled(body, theme::inner_card_radius(ROW_CORNER_TIGHTENING), fill);
        }
        RowStyle::Cards => {
            // A row is smaller than a card, so it gets a control's
            // gentler depth: a card's full shadow would smudge the gap.
            // A card's corners, but no rounder than the row's own ends.
            let ends = theme::round_ends(body.height()).nw;
            let radius = CornerRadius::same(theme::form().card_radius.min(ends));
            let fill = tinted(theme::surface(), emphasis);
            theme::paint_control(painter, body, radius, fill, None);
        }
        RowStyle::Bands => {
            let base = match shade {
                BandShade::Soft => theme::surface_muted(),
                BandShade::Surface => theme::surface(),
            };
            let radius = theme::control_radius(body.height());
            painter.rect_filled(body, radius, tinted(base, emphasis));
        }
    }
}

/// `base` leaning toward the primary color as far as `emphasis` asks.
fn tinted(base: Color32, emphasis: RowEmphasis) -> Color32 {
    let amount = match emphasis {
        RowEmphasis::Selected => SELECTED_ROW_TINT,
        RowEmphasis::Hovered => HOVERED_ROW_TINT,
        RowEmphasis::Resting => return base,
    };
    base.lerp_to_gamma(theme::primary(), amount)
}

pub fn status_text(download: &DownloadView) -> &'static str {
    if download.stopping {
        return "Stopping";
    }
    if download
        .stream
        .as_ref()
        .is_some_and(|stream| stream.merging)
    {
        return "Merging";
    }
    match download.status {
        // The full label does not fit the status column.
        DownloadStatus::WaitingForSpace { .. } => "Disk full",
        _ => download.status.label(),
    }
}

/// Whether a row can be moved within its queue, and its neighbors for that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reorder {
    Off,
    InQueue {
        /// The first download in this view, for "Move to top".
        first: DownloadId,
        /// The download shown after this row; dropping on the row's lower
        /// half puts the dragged one before it.
        next: Option<DownloadId>,
    },
}

fn row<'a>(
    ui: &mut Ui,
    list: &RowList<'a>,
    index: usize,
    download: &'a DownloadView,
    reorder: Reorder,
    actions: &mut Vec<Action>,
) {
    let RowList {
        catalog,
        columns,
        style,
        geometry,
        selection,
        ..
    } = *list;
    let selected = selection.contains(download.id);
    // Every row drags: onto a queue in the sidebar, or within a queue view.
    let (slot, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), geometry.slot_height()),
        Sense::click_and_drag(),
    );
    if !ui.is_rect_visible(slot) {
        return;
    }
    let row_id = egui::Id::new(("download-row", download.id));
    let rect = geometry.content(slot);
    let offsets = columns.offsets();
    let at = |x: f32| rect.left() + x;
    let middle = rect.center().y;

    let emphasis = if selected {
        RowEmphasis::Selected
    } else if response.hovered() {
        RowEmphasis::Hovered
    } else {
        RowEmphasis::Resting
    };
    paint_row_body(
        ui.painter(),
        style,
        geometry.body(slot),
        emphasis,
        band_shade(index),
    );

    let (fg, bg) = theme::look_colors(catalog.categories.get(download.category).look);
    let tile = Rect::from_min_size(pos2(at(8.0), middle - TILE / 2.0), vec2(TILE, TILE));
    widgets::file_tile(
        ui,
        tile,
        &format::extension_badge(&download.file_name),
        fg,
        bg,
    );
    name_cell(ui, download, pos2(at(offsets.name), middle), columns);

    if let (Some(x), Some(width)) = (offsets.size, columns.size) {
        let size = download
            .size
            .map_or_else(|| "Unknown".to_owned(), format::bytes);
        let text = elided(ui, size, theme::regular(13.0), theme::ink_soft(), width);
        ui.painter().galley(
            pos2(at(x), middle - text.size().y / 2.0),
            text,
            theme::ink_soft(),
        );
    }
    if let (Some(x), Some(width)) = (offsets.progress, columns.progress) {
        progress_cell(
            ui,
            download,
            Rect::from_min_size(pos2(at(x), rect.top()), vec2(width, ROW_HEIGHT)),
        );
    }
    if let (Some(x), Some(width)) = (offsets.speed, columns.speed) {
        speed_cell(ui, download, pos2(at(x), middle), width);
    }

    let (status_fg, status_bg) = theme::status_colors(&download.status);
    pill(
        ui,
        pos2(at(offsets.status), middle),
        status_text(download),
        status_fg,
        status_bg,
    );

    // Fixed slots so icons line up across rows even when one is absent.
    let action_slot = |index: usize| {
        let left = rect.right() - 8.0 - columns.actions.width() + index as f32 * 36.0;
        Rect::from_min_size(pos2(left, middle - 16.0), vec2(ICON_BUTTON, ICON_BUTTON))
    };
    if let Some((icon, tooltip, action)) = primary_row_action(download) {
        if icon_button_at(
            ui,
            action_slot(0),
            row_id.with("primary"),
            icon,
            &hotkeys::PRIMARY.label(tooltip),
            theme::IconButtonInk::on_page(theme::ink_soft()),
        )
        .clicked()
        {
            actions.push(action);
        }
    }
    if columns.actions == RowActions::All {
        if icon_button_at(
            ui,
            action_slot(1),
            row_id.with("reveal"),
            Icon::Folder,
            &hotkeys::SHOW_IN_FOLDER.label("Show in folder"),
            theme::IconButtonInk::on_page(theme::ink_soft()),
        )
        .clicked()
        {
            actions.push(Action::Reveal(location(download)));
        }
        if icon_button_at(
            ui,
            action_slot(2),
            row_id.with("remove"),
            Icon::Trash,
            &hotkeys::REMOVE.label("Remove"),
            theme::IconButtonInk::on_page(theme::ink_soft()),
        )
        .clicked()
        {
            actions.push(Action::AskRemove(vec![download.id]));
        }
    }

    if response.dragged() {
        let group = list.group_of(download);
        response.dnd_set_drag_payload(DraggedDownloads(
            group.iter().map(|dragged| dragged.id).collect(),
        ));
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }
    if let Reorder::InQueue { next, .. } = reorder {
        drop_to_reorder(ui, &response, slot, download.id, next, actions);
    }
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    // Right-clicking a picked row keeps the group, so its menu acts on all.
    if response.secondary_clicked() && !selected {
        actions.push(Action::Select(Some(download.id)));
    }
    response.context_menu(|ui| {
        let group = list.group_of(download);
        match group.as_slice() {
            [one] => row_menu(ui, one, catalog, reorder, actions),
            _ => group_menu(ui, &group, list, reorder, actions),
        }
    });
    if response.double_clicked() {
        if let Some(file) = download.finished_file() {
            actions.push(Action::OpenFile(file.to_path_buf()));
        }
    } else if response.clicked() {
        let pick = Pick::from_modifiers(ui.input(|input| input.modifiers));
        actions.push(Action::Pick(download.id, pick));
    }
}

/// Dropping dragged rows on a row in a queue view: a line shows where they
/// would land, and releasing moves them there together.
fn drop_to_reorder(
    ui: &Ui,
    response: &egui::Response,
    rect: Rect,
    id: DownloadId,
    next: Option<DownloadId>,
    actions: &mut Vec<Action>,
) {
    let Some(pointer) = ui.ctx().pointer_interact_pos() else {
        return;
    };
    let upper_half = pointer.y < rect.center().y;
    let place = if upper_half {
        QueuePlace::Before(id)
    } else {
        next.map_or(QueuePlace::End, QueuePlace::Before)
    };
    if let Some(dragged) = response.dnd_hover_payload::<DraggedDownloads>() {
        if !dragged.0.contains(&id) {
            let y = if upper_half {
                rect.top()
            } else {
                rect.bottom()
            };
            let painter = ui.painter();
            let stroke = egui::Stroke::new(2.0, theme::primary());
            painter.line_segment(
                [pos2(rect.left() + 10.0, y), pos2(rect.right() - 6.0, y)],
                stroke,
            );
            painter.circle_filled(pos2(rect.left() + 8.0, y), 4.0, theme::primary());
        }
    }
    if let Some(dragged) = response.dnd_release_payload::<DraggedDownloads>() {
        if !dragged.0.contains(&id) {
            actions.push(Action::MoveInQueue(dragged.0.clone(), place));
        }
    }
}

/// Where a download lands when it moves one place up or down in `visible`;
/// `None` at the ends.
pub fn place_after_step(
    visible: &[&DownloadView],
    id: DownloadId,
    step: Step,
) -> Option<QueuePlace> {
    let index = visible.iter().position(|download| download.id == id)?;
    match step {
        Step::Previous => index
            .checked_sub(1)
            .map(|previous| QueuePlace::Before(visible[previous].id)),
        Step::Next => {
            visible.get(index + 1)?;
            Some(
                visible
                    .get(index + 2)
                    .map_or(QueuePlace::End, |after| QueuePlace::Before(after.id)),
            )
        }
    }
}

/// The right-click menu of a download row.
fn row_menu(
    ui: &mut Ui,
    download: &DownloadView,
    catalog: &Catalog<'_>,
    reorder: Reorder,
    actions: &mut Vec<Action>,
) {
    ui.set_min_width(220.0);
    if let Some(file) = download.finished_file() {
        menu_item(
            ui,
            actions,
            &hotkeys::OPEN_FILE.label("Open file"),
            Action::OpenFile(file.to_path_buf()),
        );
    }
    menu_item(
        ui,
        actions,
        &hotkeys::RENAME.label("Rename file"),
        Action::AskRename(download.id),
    );
    menu_item(
        ui,
        actions,
        &hotkeys::SHOW_IN_FOLDER.label("Show in folder"),
        Action::Reveal(location(download)),
    );
    ui.separator();
    if let Some((_, label, action)) = primary_row_action(download) {
        menu_item(ui, actions, &hotkeys::PRIMARY.label(label), action);
    }
    if download.can_start_now {
        menu_item(ui, actions, "Start now", Action::StartNow(download.id));
    }
    if can_start_over(download) {
        menu_item(ui, actions, "Start over", Action::Restart(download.id));
    }
    if let Reorder::InQueue { first, .. } = reorder {
        if first != download.id {
            menu_item(
                ui,
                actions,
                "Move to top of queue",
                Action::MoveInQueue(vec![download.id], QueuePlace::Before(first)),
            );
        }
        menu_item(
            ui,
            actions,
            "Move to bottom of queue",
            Action::MoveInQueue(vec![download.id], QueuePlace::End),
        );
    }
    ui.menu_button("Move to queue", |ui| {
        for queue in catalog.queues {
            let current = queue.id == download.queue;
            if ui
                .add_enabled(!current, egui::Button::new(&queue.name))
                .clicked()
            {
                actions.push(Action::MoveToQueue(vec![download.id], queue.id));
                ui.close_menu();
            }
        }
    });
    if download.status.can_change_speed_limit() {
        ui.menu_button("Speed limit", |ui| {
            speed_limit_menu(ui, download, catalog, actions);
        });
    }
    ui.separator();
    if let Some(page) = download_page(download) {
        menu_item(ui, actions, "Open download page", Action::OpenPage(page));
    }
    if download.status != DownloadStatus::Completed && !download.status.is_active() {
        menu_item(
            ui,
            actions,
            "Refresh download address…",
            Action::RefreshAddress(download.id),
        );
    }
    menu_item(
        ui,
        actions,
        &hotkeys::COPY_ADDRESS.label("Copy address"),
        Action::CopyText(download.url.to_string()),
    );
    ui.separator();
    menu_item(
        ui,
        actions,
        &hotkeys::REMOVE.label("Remove…"),
        Action::AskRemove(vec![download.id]),
    );
}

/// Whether a download has data that starting over would throw away.
fn can_start_over(download: &DownloadView) -> bool {
    download.status.can_resume() && download.received > 0
}

/// What the right-click menu can do for several picked downloads: each
/// list holds the ones an item applies to.
#[derive(Debug, PartialEq, Eq)]
struct GroupPlan {
    ids: Vec<DownloadId>,
    resume: Vec<DownloadId>,
    pause: Vec<DownloadId>,
    start_now: Vec<DownloadId>,
    start_over: Vec<DownloadId>,
    /// The queue all of them are in already, if they share one.
    shared_queue: Option<QueueId>,
    reorder: GroupReorder,
}

/// Moving a group within the start order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupReorder {
    /// Not a queue view, so the start order is not shown.
    Off,
    /// The group is the first rows of the queue already.
    OnTop,
    /// Moving it to the top puts it at this place.
    ToTop(QueuePlace),
}

fn group_plan(group: &[&DownloadView], visible: &[&DownloadView], reorder: Reorder) -> GroupPlan {
    let ids: Vec<DownloadId> = group.iter().map(|download| download.id).collect();
    let where_ = |applies: fn(&DownloadView) -> bool| {
        group
            .iter()
            .filter(|download| applies(download))
            .map(|download| download.id)
            .collect()
    };
    // Each row's own button decides what it can do, so the group does it
    // where that button would.
    let (mut resume, mut pause) = (Vec::new(), Vec::new());
    for download in group {
        match primary_kind(download) {
            Some(PrimaryKind::Resume) => resume.push(download.id),
            Some(PrimaryKind::Pause) => pause.push(download.id),
            None => {}
        }
    }
    let first_queue = group.first().map(|download| download.queue);
    let shared_queue =
        first_queue.filter(|queue| group.iter().all(|download| download.queue == *queue));
    let reorder = match reorder {
        Reorder::Off => GroupReorder::Off,
        Reorder::InQueue { first, .. } => {
            let on_top = visible
                .iter()
                .take(group.len())
                .all(|shown| ids.contains(&shown.id));
            if on_top {
                GroupReorder::OnTop
            } else {
                GroupReorder::ToTop(QueuePlace::Before(first))
            }
        }
    };
    GroupPlan {
        resume,
        pause,
        start_now: where_(|download| download.can_start_now),
        start_over: where_(can_start_over),
        shared_queue,
        reorder,
        ids,
    }
}

/// The right-click menu for several picked downloads: the same items as for
/// one, in the same places, with those that need a single file turned off.
fn group_menu(
    ui: &mut Ui,
    group: &[&DownloadView],
    list: &RowList<'_>,
    reorder: Reorder,
    actions: &mut Vec<Action>,
) {
    ui.set_min_width(220.0);
    let plan = group_plan(group, list.visible, reorder);
    let each = |ids: &[DownloadId], action: fn(DownloadId) -> Action| -> Vec<Action> {
        ids.iter().copied().map(action).collect()
    };
    ui.label(
        egui::RichText::new(format!("{} downloads selected", group.len()))
            .font(theme::semibold(12.5))
            .color(theme::ink_soft()),
    );
    ui.separator();
    single_file_item(ui, &hotkeys::OPEN_FILE.label("Open file"));
    single_file_item(ui, &hotkeys::RENAME.label("Rename file"));
    single_file_item(ui, &hotkeys::SHOW_IN_FOLDER.label("Show in folder"));
    ui.separator();
    group_item(
        ui,
        actions,
        GroupItem {
            label: "Resume",
            does: each(&plan.resume, Action::Resume),
            off_because: Some("None of them is paused or failed"),
        },
    );
    group_item(
        ui,
        actions,
        GroupItem {
            label: "Pause",
            does: each(&plan.pause, Action::Pause),
            off_because: Some("None of them is downloading or waiting"),
        },
    );
    group_item(
        ui,
        actions,
        GroupItem {
            label: "Start now",
            does: each(&plan.start_now, Action::StartNow),
            off_because: Some("None of them is waiting for its queue"),
        },
    );
    group_item(
        ui,
        actions,
        GroupItem {
            label: "Start over",
            does: each(&plan.start_over, Action::Restart),
            off_because: Some("None of them has data to start over from"),
        },
    );
    let to_top = match plan.reorder {
        GroupReorder::Off => None,
        GroupReorder::OnTop => Some(Vec::new()),
        GroupReorder::ToTop(place) => Some(vec![Action::MoveInQueue(plan.ids.clone(), place)]),
    };
    if let Some(to_top) = to_top {
        group_item(
            ui,
            actions,
            GroupItem {
                label: "Move to top of queue",
                does: to_top,
                off_because: Some("Already at the top of the queue"),
            },
        );
        group_item(
            ui,
            actions,
            GroupItem {
                label: "Move to bottom of queue",
                does: vec![Action::MoveInQueue(plan.ids.clone(), QueuePlace::End)],
                off_because: None,
            },
        );
    }
    ui.menu_button("Move to queue", |ui| {
        for queue in list.catalog.queues {
            let all_there = plan.shared_queue == Some(queue.id);
            if ui
                .add_enabled(!all_there, egui::Button::new(&queue.name))
                .on_disabled_hover_text("They are all in this queue")
                .clicked()
            {
                actions.push(Action::MoveToQueue(plan.ids.clone(), queue.id));
                ui.close_menu();
            }
        }
    });
    single_file_item(ui, "Speed limit");
    ui.separator();
    single_file_item(ui, "Open download page");
    single_file_item(ui, "Refresh download address…");
    let addresses = group
        .iter()
        .map(|download| download.url.to_string())
        .collect();
    group_item(
        ui,
        actions,
        GroupItem {
            label: "Copy addresses",
            does: vec![Action::CopyAddresses(addresses)],
            off_because: None,
        },
    );
    ui.separator();
    group_item(
        ui,
        actions,
        GroupItem {
            label: &hotkeys::REMOVE.label("Remove…"),
            does: vec![Action::AskRemove(plan.ids.clone())],
            off_because: None,
        },
    );
}

/// One group menu item: what it does, and why it is off when it does
/// nothing (`None` for an item that always does something).
struct GroupItem<'a> {
    label: &'a str,
    does: Vec<Action>,
    off_because: Option<&'a str>,
}

fn group_item(ui: &mut Ui, actions: &mut Vec<Action>, item: GroupItem<'_>) {
    let applies = !item.does.is_empty();
    let mut button = ui.add_enabled(applies, egui::Button::new(item.label));
    if let Some(reason) = item.off_because {
        button = button.on_disabled_hover_text(reason);
    }
    if button.clicked() {
        actions.extend(item.does);
        ui.close_menu();
    }
}

/// A menu item that needs one download, shown turned off for a group.
fn single_file_item(ui: &mut Ui, label: &str) {
    ui.add_enabled(false, egui::Button::new(label))
        .on_disabled_hover_text("Select a single download for this");
}

/// Own limits offered for a download, in KiB/s.
const OWN_LIMITS_KIB: [u32; 6] = [256, 512, 1024, 2048, 5120, 10240];

/// The limits a download's menu offers: the global one, none, and a few
/// of its own, plus the current one if it is not among them.
fn speed_limit_choices(current: DownloadLimit) -> Vec<DownloadLimit> {
    let mut choices = vec![DownloadLimit::FollowSettings, DownloadLimit::Unlimited];
    choices.extend(
        OWN_LIMITS_KIB
            .into_iter()
            .filter_map(NonZeroU32::new)
            .map(|kib_per_second| DownloadLimit::Limited { kib_per_second }),
    );
    if !choices.contains(&current) {
        choices.push(current);
    }
    choices
}

/// Picks how fast one download may go; the current choice is highlighted.
pub fn speed_limit_menu(
    ui: &mut Ui,
    download: &DownloadView,
    catalog: &Catalog<'_>,
    actions: &mut Vec<Action>,
) {
    ui.set_min_width(190.0);
    for limit in speed_limit_choices(download.speed_limit) {
        let label = format::download_limit(limit, catalog.global_limit_kib_per_second);
        if ui
            .selectable_label(download.speed_limit == limit, label)
            .clicked()
        {
            actions.push(Action::SetSpeedLimit(download.id, limit));
            ui.close_menu();
        }
    }
}

fn menu_item(ui: &mut Ui, actions: &mut Vec<Action>, label: &str, action: Action) {
    if ui.button(label).clicked() {
        actions.push(action);
        ui.close_menu();
    }
}

/// The progress height the name cell's inline progress was laid out for.
const INLINE_PROGRESS_ROOM: f32 = 6.0;
/// Where the inline progress starts, below the name cell's center line.
const INLINE_PROGRESS_TOP: f32 = 19.5;

/// File name over either the source host or, when progress has no column of
/// its own, a progress line and a thin bar.
fn name_cell(ui: &Ui, download: &DownloadView, left_center: egui::Pos2, columns: &Columns) {
    let width = columns.name;
    let name = elided(
        ui,
        &download.file_name,
        theme::semibold(14.0),
        theme::ink(),
        width,
    );
    let subtitle = if columns.progress.is_some() {
        let host = download.url.host_str().unwrap_or("unknown host");
        format!(
            "{host} · {}",
            format::added_at(download.added_at_unix, unix_now())
        )
    } else {
        inline_progress_text(download)
    };
    let subtitle = elided(
        ui,
        subtitle,
        theme::regular(12.0),
        theme::ink_faint(),
        width,
    );
    let inline_progress =
        columns.progress.is_none() && download.status != DownloadStatus::Completed;
    // A progress style taller than the bar lifts the text by half the
    // difference, so the name, subtitle, and progress stay centered.
    let lift = if inline_progress {
        ((widgets::progress_height() - INLINE_PROGRESS_ROOM) / 2.0).max(0.0)
    } else {
        0.0
    };
    let left_center = left_center - vec2(0.0, lift);
    ui.painter().galley(
        pos2(left_center.x, left_center.y - name.size().y - 1.0),
        name,
        theme::ink(),
    );
    ui.painter().galley(
        pos2(left_center.x, left_center.y + 2.0),
        subtitle,
        theme::ink_faint(),
    );
    if inline_progress {
        let bar = Rect::from_min_size(
            pos2(left_center.x, left_center.y + INLINE_PROGRESS_TOP),
            vec2(width, widgets::progress_height()),
        );
        progress_bar(
            ui.painter(),
            bar,
            download.fraction().unwrap_or(0.0),
            theme::status_colors(&download.status).0,
        );
    }
}

fn inline_progress_text(download: &DownloadView) -> String {
    let amount = match download.size {
        Some(size) if download.status == DownloadStatus::Completed => format::bytes(size),
        Some(size) => format!(
            "{} of {}",
            format::percent(download.fraction().unwrap_or(0.0)),
            format::bytes(size)
        ),
        None => format::bytes(download.received),
    };
    if download.status.is_active() {
        format!("{amount} · {}", format::speed(download.bytes_per_second))
    } else {
        amount
    }
}

/// Where "Show in folder" points: the file once its name is known, else
/// the folder it is saved into.
pub fn location(download: &DownloadView) -> std::path::PathBuf {
    download
        .target
        .clone()
        .unwrap_or_else(|| download.directory.clone())
}

/// What a download's main button does in its current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrimaryKind {
    Pause,
    /// Resume, retry, or try again after the disk filled up.
    Resume,
}

fn primary_kind(download: &DownloadView) -> Option<PrimaryKind> {
    if download.stopping {
        return None;
    }
    match download.status {
        DownloadStatus::Queued | DownloadStatus::Connecting | DownloadStatus::Downloading => {
            Some(PrimaryKind::Pause)
        }
        DownloadStatus::Paused
        | DownloadStatus::Failed { .. }
        | DownloadStatus::WaitingForSpace { .. } => Some(PrimaryKind::Resume),
        DownloadStatus::Completed => None,
    }
}

// The pause/resume/retry button offered for a download in its current state.
pub fn primary_row_action(download: &DownloadView) -> Option<(Icon, &'static str, Action)> {
    let id = download.id;
    Some(match primary_kind(download)? {
        PrimaryKind::Pause => (Icon::Pause, "Pause", Action::Pause(id)),
        PrimaryKind::Resume => match download.status {
            DownloadStatus::Failed { .. } => (Icon::Refresh, "Retry", Action::Resume(id)),
            DownloadStatus::WaitingForSpace { .. } => {
                (Icon::Refresh, "Try again now", Action::Resume(id))
            }
            _ => (Icon::Play, "Resume", Action::Resume(id)),
        },
    })
}

fn progress_cell(ui: &Ui, download: &DownloadView, cell: Rect) {
    let middle = cell.center().y;
    let color = theme::status_colors(&download.status).0;
    let label = match download.fraction() {
        Some(fraction) => format::percent(fraction),
        None => format::bytes(download.received),
    };
    let detail = match (download.size, &download.status) {
        (_, DownloadStatus::Completed) => String::new(),
        (Some(size), _) => format!(
            "{} / {}",
            format::bytes(download.received),
            format::bytes(size)
        ),
        (None, _) => String::new(),
    };
    let label = galley(ui, label, theme::semibold(12.0), theme::ink());
    let detail = elided(
        ui,
        detail,
        theme::regular(11.5),
        theme::ink_faint(),
        cell.width() - label.size().x - 8.0,
    );
    ui.painter().galley(
        pos2(cell.left(), middle - 6.0 - label.size().y),
        label,
        theme::ink(),
    );
    ui.painter().galley(
        pos2(
            cell.right() - detail.size().x,
            middle - 6.0 - detail.size().y,
        ),
        detail,
        theme::ink_faint(),
    );
    // Taller styles (a knob, a waveform) grow downward, into the room the
    // row keeps below the bar, so they stay clear of the text above.
    let bar = Rect::from_min_size(
        pos2(cell.left(), middle + 2.0),
        vec2(cell.width(), widgets::progress_height()),
    );
    progress_bar(ui.painter(), bar, download.fraction().unwrap_or(0.0), color);
}

fn speed_cell(ui: &Ui, download: &DownloadView, left_center: egui::Pos2, width: f32) {
    let active = download.status.is_active();
    let speed = if active {
        format::speed(download.bytes_per_second)
    } else {
        "—".to_owned()
    };
    let eta = match (active, download.eta()) {
        (true, Some(eta)) => format!("{} left", format::duration(eta)),
        _ => String::new(),
    };
    let speed = elided(ui, speed, theme::semibold(13.0), theme::ink(), width);
    let eta = elided(ui, eta, theme::regular(11.5), theme::ink_faint(), width);
    let top = if eta.size().x > 0.0 {
        left_center.y - speed.size().y
    } else {
        left_center.y - speed.size().y / 2.0
    };
    ui.painter()
        .galley(pos2(left_center.x, top), speed, theme::ink());
    ui.painter().galley(
        pos2(left_center.x, left_center.y + 1.0),
        eta,
        theme::ink_faint(),
    );
}

fn empty_state(ui: &mut Ui, filter: Filter, searching: bool, actions: &mut Vec<Action>) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() / 2.0 - 110.0).max(24.0));
        let (circle, _) = ui.allocate_exact_size(vec2(84.0, 84.0), Sense::hover());
        ui.painter()
            .circle_filled(circle.center(), 42.0, theme::primary_soft());
        Icon::Download.paint(ui.painter(), circle.shrink(26.0), theme::primary());
        ui.add_space(12.0);
        let (title, body) = if searching {
            ("No matches", "Try a different file name or address.")
        } else if matches!(filter, Filter::All | Filter::Queue(_)) {
            (
                "No downloads yet",
                "Paste a link or send one from your browser to get started.",
            )
        } else {
            ("Nothing here", "Downloads in this view will show up here.")
        };
        ui.label(
            egui::RichText::new(title)
                .font(theme::bold(18.0))
                .color(theme::ink()),
        );
        ui.label(
            egui::RichText::new(body)
                .font(theme::regular(13.5))
                .color(theme::ink_soft()),
        );
        if matches!(filter, Filter::All | Filter::Queue(_)) && !searching {
            ui.add_space(12.0);
            if button(ui, ButtonKind::Primary, Some(Icon::Plus), "Add URL").clicked() {
                actions.push(Action::OpenAddUrl);
            }
        }
    });
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::view;
    use super::*;

    #[test]
    fn the_speed_menu_offers_global_none_and_own_limits_once() {
        let mib = DownloadLimit::Limited {
            kib_per_second: NonZeroU32::new(1024).unwrap(),
        };
        let choices = speed_limit_choices(mib);
        assert_eq!(choices[0], DownloadLimit::FollowSettings);
        assert_eq!(choices[1], DownloadLimit::Unlimited);
        assert_eq!(choices.iter().filter(|&&choice| choice == mib).count(), 1);
        let odd = DownloadLimit::Limited {
            kib_per_second: NonZeroU32::new(300).unwrap(),
        };
        assert_eq!(speed_limit_choices(odd).last(), Some(&odd));
    }

    fn named(id: u64, name: &str) -> DownloadView {
        DownloadView {
            file_name: name.to_owned(),
            ..view(id)
        }
    }

    fn ids(downloads: &[&DownloadView]) -> Vec<u64> {
        downloads.iter().map(|download| download.id.0).collect()
    }

    fn sorted_by(
        downloads: &[DownloadView],
        column: SortColumn,
        direction: SortDirection,
    ) -> Vec<u64> {
        let order = ListOrder::Column { column, direction };
        ids(&visible_downloads(downloads, Filter::All, "", order))
    }

    #[test]
    fn names_sort_ignoring_case() {
        let downloads = [
            named(1, "beta.zip"),
            named(2, "Alpha.zip"),
            named(3, "gamma.zip"),
        ];
        assert_eq!(
            sorted_by(&downloads, SortColumn::Name, SortDirection::Ascending),
            [2, 1, 3]
        );
        assert_eq!(
            sorted_by(&downloads, SortColumn::Name, SortDirection::Descending),
            [3, 1, 2]
        );
    }

    #[test]
    fn unknown_sizes_go_last_in_either_direction() {
        let sized = |id, size| DownloadView { size, ..view(id) };
        let downloads = [sized(1, None), sized(2, Some(10)), sized(3, Some(30))];
        assert_eq!(
            sorted_by(&downloads, SortColumn::Size, SortDirection::Ascending),
            [2, 3, 1]
        );
        assert_eq!(
            sorted_by(&downloads, SortColumn::Size, SortDirection::Descending),
            [3, 2, 1]
        );
    }

    #[test]
    fn status_sorts_active_downloads_first_and_ties_keep_the_start_order() {
        let with = |id, status| DownloadView { status, ..view(id) };
        let downloads = [
            with(1, DownloadStatus::Completed),
            with(2, DownloadStatus::Paused),
            with(3, DownloadStatus::Downloading),
            with(4, DownloadStatus::Paused),
        ];
        assert_eq!(
            sorted_by(&downloads, SortColumn::Status, SortDirection::Ascending),
            [3, 2, 4, 1]
        );
    }

    #[test]
    fn the_start_order_shows_the_list_as_it_is() {
        let downloads = [named(1, "b"), named(2, "a")];
        assert_eq!(
            ids(&visible_downloads(
                &downloads,
                Filter::All,
                "",
                ListOrder::StartOrder
            )),
            [1, 2]
        );
    }

    #[test]
    fn a_queue_view_keeps_its_start_order_whatever_the_sort() {
        let downloads = [named(1, "b"), named(2, "a")];
        let order = ListOrder::Column {
            column: SortColumn::Name,
            direction: SortDirection::Ascending,
        };
        let queue = Filter::Queue(downpour_core::QueueId::MAIN);
        assert_eq!(
            ids(&visible_downloads(&downloads, queue, "", order)),
            [1, 2]
        );
    }

    #[test]
    fn arrows_walk_the_list_and_stop_at_its_ends() {
        let views: Vec<DownloadView> = (1..=3).map(view).collect();
        let visible: Vec<&DownloadView> = views.iter().collect();
        let step = |current, step| neighbor(&visible, current, step).map(|d| d.id);
        assert_eq!(step(None, Step::Next), Some(DownloadId(1)));
        assert_eq!(step(None, Step::Previous), Some(DownloadId(3)));
        assert_eq!(step(Some(DownloadId(1)), Step::Next), Some(DownloadId(2)));
        assert_eq!(step(Some(DownloadId(3)), Step::Next), Some(DownloadId(3)));
        assert_eq!(
            step(Some(DownloadId(1)), Step::Previous),
            Some(DownloadId(1))
        );
        // A selection that is no longer shown restarts from the top.
        assert_eq!(step(Some(DownloadId(9)), Step::Next), Some(DownloadId(1)));
    }

    #[test]
    fn one_step_moves_land_between_the_right_neighbors() {
        let views: Vec<DownloadView> = (1..=3).map(view).collect();
        let visible: Vec<&DownloadView> = views.iter().collect();
        let place = |id, step| place_after_step(&visible, DownloadId(id), step);
        assert_eq!(
            place(2, Step::Previous),
            Some(QueuePlace::Before(DownloadId(1)))
        );
        assert_eq!(
            place(1, Step::Next),
            Some(QueuePlace::Before(DownloadId(3)))
        );
        assert_eq!(place(2, Step::Next), Some(QueuePlace::End));
        assert_eq!(place(1, Step::Previous), None);
        assert_eq!(place(3, Step::Next), None);
        assert_eq!(place(9, Step::Next), None);
    }

    #[test]
    fn row_cards_keep_their_gap_inside_the_hit_target() {
        let geometry = RowGeometry::for_style(RowStyle::Cards);
        let slot = Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, geometry.slot_height()));
        let body = geometry.body(slot);
        assert_eq!(body.height(), ROW_HEIGHT);
        assert_eq!(body.top() - slot.top(), slot.bottom() - body.bottom());
        assert_eq!(body.width(), slot.width());
        let content = geometry.content(slot);
        assert_eq!(content.height(), ROW_HEIGHT);
        assert_eq!(content.width(), geometry.content_width(slot.width()));
        assert!(body.contains_rect(content));
    }

    #[test]
    fn lines_and_bands_rows_fill_their_slot() {
        for style in [RowStyle::Lines, RowStyle::Bands] {
            let geometry = RowGeometry::for_style(style);
            let slot = Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, geometry.slot_height()));
            assert_eq!(geometry.content(slot), slot, "{style:?}");
            assert_eq!(geometry.edge_room(), 0.0, "{style:?}");
        }
    }

    #[test]
    fn bands_alternate_starting_soft() {
        let shades: Vec<BandShade> = (0..4).map(band_shade).collect();
        assert_eq!(
            shades,
            [
                BandShade::Soft,
                BandShade::Surface,
                BandShade::Soft,
                BandShade::Surface
            ]
        );
    }

    #[test]
    fn arrows_on_an_empty_list_select_nothing() {
        assert!(neighbor(&[], None, Step::Previous).is_none());
        assert!(neighbor(&[], None, Step::Next).is_none());
    }

    fn in_queue(first: u64) -> Reorder {
        Reorder::InQueue {
            first: DownloadId(first),
            next: None,
        }
    }

    #[test]
    fn a_group_menu_does_each_thing_where_it_applies() {
        let paused = DownloadView {
            status: DownloadStatus::Paused,
            received: 10,
            ..view(1)
        };
        let running = DownloadView {
            status: DownloadStatus::Downloading,
            ..view(2)
        };
        let finished = DownloadView {
            status: DownloadStatus::Completed,
            ..view(3)
        };
        let group = [&paused, &running, &finished];
        let plan = group_plan(&group, &group, Reorder::Off);
        assert_eq!(plan.resume, [DownloadId(1)]);
        assert_eq!(plan.pause, [DownloadId(2)]);
        assert_eq!(plan.start_over, [DownloadId(1)]);
        assert!(plan.start_now.is_empty());
        assert_eq!(plan.reorder, GroupReorder::Off);
    }

    #[test]
    fn a_group_on_top_of_its_queue_cannot_move_to_the_top() {
        let (one, two, three) = (view(1), view(2), view(3));
        let visible = [&one, &two, &three];
        assert_eq!(
            group_plan(&[&one, &two], &visible, in_queue(1)).reorder,
            GroupReorder::OnTop
        );
        assert_eq!(
            group_plan(&[&two, &three], &visible, in_queue(1)).reorder,
            GroupReorder::ToTop(QueuePlace::Before(DownloadId(1)))
        );
    }

    #[test]
    fn a_group_shares_a_queue_only_when_all_are_in_it() {
        let (one, two) = (view(1), view(2));
        let elsewhere = DownloadView {
            queue: QueueId(7),
            ..view(3)
        };
        assert_eq!(
            group_plan(&[&one, &two], &[&one, &two], Reorder::Off).shared_queue,
            Some(QueueId::MAIN)
        );
        assert_eq!(
            group_plan(&[&one, &elsewhere], &[&one, &elsewhere], Reorder::Off).shared_queue,
            None
        );
    }
}

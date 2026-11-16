//! The pages: Containers (with a detail pane for logs, inspect, stats and exec),
//! Images, Volumes, Networks and Settings.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use noa_gpui_platform::noa_gpui_core::style::{FlexDirection, Size, length, percent};
use noa_gpui_platform::noa_gpui_core::widgets::{
    Button, ButtonClicked, CellActionClicked, CellButton, CellLook, CheckState, Checkbox,
    CheckboxChanged, ColumnAlign, ColumnSort, Container as Boxed, DescriptionList,
    DescriptionListParts, FindAction, FindOptions, FindStatus, GroupActionClicked, GroupRowClicked,
    Label, LogLine, LogView, RowSelectionChanged, RowSeparator, SearchField, SearchSubmitted,
    SegmentMode, SegmentStyle, SegmentedControl, SelectionChanged, TableColumn, TableData,
    TableView, Tag, TagStyle, Terminal, TerminalInput, TextChanged, TextField, TextSubmitted,
    TextView, Tone,
};
use noa_gpui_platform::noa_gpui_core::{Key, Message, Role, Shortcut, Style, UiTree, WidgetId};
use noa_gpui_platform::noa_gpui_scene::VectorIcon;

use super::icons::icons;
use super::model::{self, LogBuffer, Row};
use super::stack_map::{self, MapSheet};
use super::{Act, Ctx, Page, tokens, widgets};
use crate::backend::{Backend, UiEvent};
use crate::commands;
use crate::config::{LOG_LINES_MAX, LOG_LINES_MIN, LanguagePref, ThemePref};
use crate::dto::{
    ComposeCommand, ConnState, Container, ContainerAction, ContainerState, ExecOutput, ExecShell,
    Freshness, InspectKind, LogStream, PruneKind, RestartChoice, RunForm, RunResult, RunTemplate,
    StatsEvent, UiError, UpdateCheck,
};
use crate::run_spec;

/// Height of the compose output under the containers list.
const COMPOSE_HEIGHT: f32 = 190.0;
use tokens::{GROUP_BUTTON, ICON_BUTTON, LIST_ROW};
/// Room for a running container's seven icon buttons (the table puts 4 px between
/// buttons and 8 px at each side of a cell).
const ACTIONS_WIDTH: f32 = 7.0 * ICON_BUTTON + 6.0 * tokens::SPACE_1 + 2.0 * tokens::SPACE_3;

/// The width of a list's last column for `n` icon buttons, with room for its title.
fn actions_width(n: usize) -> f32 {
    let n = n.max(1) as f32;
    n * ICON_BUTTON + (n - 1.0) * tokens::SPACE_1 + 2.0 * tokens::SPACE_3 + tokens::SPACE_3
}

/// What a row button does, for its tooltip (Docker's own verbs stay in English).
fn action_name(t: crate::ui::i18n::Tr, action: &str) -> String {
    match action {
        "logs" => t.tab_logs().to_owned(),
        "remove" => t.remove().to_owned(),
        "start" => "Start".into(),
        "stop" => "Stop".into(),
        "pause" => "Pause".into(),
        "resume" => "Resume".into(),
        "restart" => "Restart".into(),
        "kill" => "Kill".into(),
        "exec" => "Exec".into(),
        "inspect" => "Inspect".into(),
        "run" => t.run().to_owned(),
        other => other.into(),
    }
}

/// An icon button in a table row, named in its tooltip.
fn row_button(t: crate::ui::i18n::Tr, action: &'static str, icon: &VectorIcon) -> CellButton {
    CellButton::new(action)
        .icon(icon.clone())
        .tooltip(action_name(t, action))
        .height(ICON_BUTTON)
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

// ---- tables ----

/// One row of a [`Grid`]: its cells, how each is drawn, and the key that identifies
/// it across updates.
#[derive(Debug, Clone, PartialEq)]
struct GridRow {
    cells: Vec<String>,
    looks: Vec<CellLook>,
    /// The buttons of the row's [`CellLook::Buttons`] cell.
    buttons: Vec<CellButton>,
    /// A heading for the rows after it (a compose stack).
    group: bool,
    /// A group row's own buttons, at the row's trailing edge.
    group_buttons: Vec<CellButton>,
    /// A second, smaller line under a cell's text.
    details: Vec<Option<String>>,
    /// Smaller text after a cell's text, and a colored dot before it (column, value).
    note: Option<(usize, String)>,
    dot: Option<(usize, Tone)>,
    separator: RowSeparator,
    /// What a [`ColumnSort::Number`] column sorts a cell by (bytes, a time), where its
    /// text does not sort ("1.2 GB", "2 hours ago").
    sort_numbers: Vec<Option<f64>>,
    key: String,
}

impl GridRow {
    fn new(key: impl Into<String>, cells: Vec<String>) -> Self {
        Self {
            looks: vec![CellLook::Text; cells.len()],
            cells,
            buttons: Vec::new(),
            group: false,
            group_buttons: Vec::new(),
            details: Vec::new(),
            note: None,
            dot: None,
            separator: RowSeparator::Line,
            sort_numbers: Vec::new(),
            key: key.into(),
        }
    }

    fn buttons(mut self, column: usize, buttons: Vec<CellButton>) -> Self {
        self.buttons = buttons;
        self.look(column, CellLook::Buttons)
    }

    fn group(mut self, buttons: Vec<CellButton>) -> Self {
        self.group = true;
        self.group_buttons = buttons;
        self
    }

    fn detail(mut self, column: usize, text: impl Into<String>) -> Self {
        if self.details.len() <= column {
            self.details.resize(column + 1, None);
        }
        let text = text.into();
        self.details[column] = (!text.is_empty()).then_some(text);
        self
    }

    fn sort_number(mut self, column: usize, value: Option<f64>) -> Self {
        if self.sort_numbers.len() <= column {
            self.sort_numbers.resize(column + 1, None);
        }
        self.sort_numbers[column] = value;
        self
    }

    fn note(mut self, column: usize, text: impl Into<String>) -> Self {
        self.note = Some((column, text.into()));
        self
    }

    fn dot(mut self, column: usize, tone: Tone) -> Self {
        self.dot = Some((column, tone));
        self
    }

    fn look(mut self, column: usize, look: CellLook) -> Self {
        if let Some(l) = self.looks.get_mut(column) {
            *l = look;
        }
        self
    }
}

/// The table's data. The key is served as one more (hidden) column, so the table can
/// keep the selection on rows with the same key when the data is replaced.
struct GridData {
    rows: Vec<GridRow>,
    key_column: usize,
}

impl TableData for GridData {
    fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn cell(&self, row: usize, column: usize) -> Cow<'_, str> {
        let Some(r) = self.rows.get(row) else {
            return Cow::Borrowed("");
        };
        if column == self.key_column {
            return Cow::Borrowed(&r.key);
        }
        r.cells
            .get(column)
            .map_or(Cow::Borrowed(""), |c| Cow::Borrowed(c.as_str()))
    }

    fn cell_look(&self, row: usize, column: usize) -> CellLook {
        self.rows
            .get(row)
            .and_then(|r| r.looks.get(column))
            .copied()
            .unwrap_or_default()
    }

    fn is_group_row(&self, row: usize) -> bool {
        self.rows.get(row).is_some_and(|r| r.group)
    }

    fn sort_number(&self, row: usize, column: usize) -> Option<f64> {
        *self.rows.get(row)?.sort_numbers.get(column)?
    }

    fn cell_buttons(&self, row: usize, _column: usize) -> Vec<CellButton> {
        self.rows
            .get(row)
            .map(|r| r.buttons.clone())
            .unwrap_or_default()
    }

    fn cell_detail(&self, row: usize, column: usize) -> Option<Cow<'_, str>> {
        self.rows
            .get(row)?
            .details
            .get(column)?
            .as_deref()
            .map(Cow::Borrowed)
    }

    fn group_row_buttons(&self, row: usize) -> Vec<CellButton> {
        self.rows
            .get(row)
            .map(|r| r.group_buttons.clone())
            .unwrap_or_default()
    }

    fn cell_note(&self, row: usize, column: usize) -> Option<Cow<'_, str>> {
        match &self.rows.get(row)?.note {
            Some((c, text)) if *c == column => Some(Cow::Borrowed(text.as_str())),
            _ => None,
        }
    }

    fn cell_dot(&self, row: usize, column: usize) -> Option<Tone> {
        match self.rows.get(row)?.dot {
            Some((c, tone)) if c == column => Some(tone),
            _ => None,
        }
    }

    fn row_separator(&self, row: usize) -> RowSeparator {
        self.rows
            .get(row)
            .map_or(RowSeparator::Line, |r| r.separator)
    }
}

/// A table that fills its parent's height and whose rows are replaced from
/// snapshots, keeping the selection by key. Every row starts with a checkbox that
/// shows whether it is selected and adds it to the selection or takes it out; the
/// header's selects every row or none.
struct Grid {
    id: WidgetId,
    columns: usize,
    rows: Vec<GridRow>,
    selected: Option<String>,
}

impl Grid {
    /// A list whose last column (the row buttons) stays at the right edge.
    fn new(tree: &mut UiTree, parent: WidgetId, columns: Vec<TableColumn>) -> Self {
        let count = columns.len();
        let data = GridData {
            rows: Vec::new(),
            key_column: count,
        };
        let id = tree.insert(
            Some(parent),
            TableView::new(columns, data)
                .fill_height()
                .row_height(LIST_ROW)
                .bordered(false)
                .background(Role::Background)
                .row_separators(true)
                .cell_padding(tokens::CELL_PAD)
                .content_inset(tokens::LIST_INSET)
                .frozen_trailing_columns(1)
                .alternating(false)
                .highlights_hovered_row(true)
                .selection_checkboxes(true)
                .header_uppercase(true)
                .header_font_family(widgets::MONO)
                .header_background(Role::SurfaceAlt)
                .group_row_cells(true)
                .group_member_indent(36.0)
                .group_member_indent_column(1),
        );
        Self {
            id,
            columns: count,
            rows: Vec::new(),
            selected: None,
        }
    }

    fn set_rows(&mut self, tree: &mut UiTree, rows: Vec<GridRow>) {
        if rows == self.rows {
            return;
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|k| !rows.iter().any(|r| &r.key == k))
        {
            self.selected = None;
        }
        self.rows = rows;
        let data = GridData {
            rows: self.rows.clone(),
            key_column: self.columns,
        };
        let key_column = self.columns;
        tree.update::<TableView, _>(self.id, |t, cx| {
            t.set_data_keeping_selection(cx, data, |d, row| d.cell(row, key_column).into_owned());
        });
    }

    /// Reads the selection after the table reported a change.
    fn sync_selection(&mut self, tree: &UiTree) {
        self.selected = tree
            .get::<TableView>(self.id)
            .and_then(|t| t.selected_rows().first().copied())
            .and_then(|i| self.rows.get(i))
            .map(|r| r.key.clone());
    }

    fn clear_selection(&mut self, tree: &mut UiTree) {
        self.selected = None;
        tree.update::<TableView, _>(self.id, |t, cx| t.select_rows(cx, std::iter::empty()));
    }

    fn key_at(&self, row: usize) -> Option<&str> {
        self.rows.get(row).map(|r| r.key.as_str())
    }

    /// The keys of every selected row, in data order.
    fn selected_keys(&self, tree: &UiTree) -> Vec<String> {
        tree.get::<TableView>(self.id)
            .map(|t| t.selected_rows())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|i| self.key_at(i).map(str::to_owned))
            .collect()
    }

    /// Rows the table can select (not group rows).
    fn selectable_count(&self) -> usize {
        self.rows.iter().filter(|r| !r.group).count()
    }

    /// Selects every row (group rows are skipped by the table).
    fn select_all(&mut self, tree: &mut UiTree) {
        let rows = 0..self.rows.len();
        tree.update::<TableView, _>(self.id, |t, cx| t.select_rows(cx, rows));
        self.sync_selection(tree);
    }
}

/// A usage cell: "in use · n" as a green badge, or a plain "unused".
fn usage_look(users: usize) -> CellLook {
    if users > 0 {
        CellLook::StrongOutlinedBadge(Tone::Success)
    } else {
        CellLook::Colored(Tone::Neutral)
    }
}

fn state_tone(state: ContainerState) -> Tone {
    match state {
        ContainerState::Running => Tone::Success,
        ContainerState::Paused | ContainerState::Restarting | ContainerState::Removing => {
            Tone::Warning
        }
        ContainerState::Dead => Tone::Critical,
        ContainerState::Created | ContainerState::Exited | ContainerState::Unknown => Tone::Neutral,
    }
}

/// A box of fixed height whose children stack vertically (a pane under a table).
fn fixed_pane(tree: &mut UiTree, parent: WidgetId, height: f32) -> WidgetId {
    tree.insert(
        Some(parent),
        Boxed::new(Style {
            flex_direction: FlexDirection::Column,
            gap: widgets::spacing(tokens::SPACE_2),
            size: Size {
                width: percent(1.0),
                height: length(height),
            },
            flex_shrink: 0.0,
            ..Default::default()
        }),
    )
}

fn label(tree: &mut UiTree, parent: WidgetId, text: impl Into<String>) -> WidgetId {
    tree.insert(Some(parent), Label::new(text))
}

fn muted(tree: &mut UiTree, parent: WidgetId, text: impl Into<String>) -> WidgetId {
    tree.insert(
        Some(parent),
        Label::new(text)
            .font_size(tokens::TEXT_SM)
            .color(Role::Muted),
    )
}

fn button(tree: &mut UiTree, parent: WidgetId, text: impl Into<String>) -> WidgetId {
    tree.insert(Some(parent), Button::new(text))
}

/// The page title with the page's controls after it, over a dividing line.
/// Controls added after a [`widgets::spacer`] go to the row's far end.
fn header(cx: &mut Ctx, root: WidgetId, title: &str) -> WidgetId {
    let row = cx.tree.insert(Some(root), widgets::bar(tokens::SPACE_3));
    cx.tree.insert(
        Some(row),
        Label::new(title)
            .font_size(tokens::TEXT_LG)
            .weight(tokens::WEIGHT_STRONG),
    );
    cx.tree
        .insert(Some(root), widgets::rule(tokens::RULE, Role::Separator));
    row
}

/// A muted line of text in its own padded row, shown only while it has text.
struct Note {
    row: WidgetId,
    label: WidgetId,
}

impl Note {
    fn new(tree: &mut UiTree, parent: WidgetId) -> Self {
        let row = tree.insert(Some(parent), widgets::bar(tokens::SPACE_3));
        let label = muted(tree, row, "");
        tree.set_hidden(row, true);
        Self { row, label }
    }

    fn set(&self, cx: &mut Ctx, text: &str) {
        cx.set_label(self.label, text);
        cx.show(self.row, !text.is_empty());
    }
}

/// A list beside a pane: returns the column the list goes in. The pane is added
/// to the returned row's parent with [`side_pane`].
fn split(tree: &mut UiTree, root: WidgetId) -> (WidgetId, WidgetId) {
    let body = tree.insert(Some(root), widgets::fill_row(0.0));
    let list = tree.insert(Some(body), widgets::fill_column(tokens::SPACE_3, 0.0));
    (body, list)
}

/// The pane at the right of `body` (hidden until shown): returns the pane and the
/// column its content goes in.
fn side_pane(tree: &mut UiTree, body: WidgetId) -> (WidgetId, WidgetId) {
    let pane = tree.insert(
        Some(body),
        widgets::side_pane(tokens::PANE_SHARE, tokens::PANE_MIN),
    );
    tree.insert(
        Some(pane),
        widgets::vrule(tokens::RULE_STRONG, Role::Outline),
    );
    let inner = tree.insert(
        Some(pane),
        widgets::fill_column(tokens::SPACE_2, tokens::SPACE_3),
    );
    tree.set_hidden(pane, true);
    (pane, inner)
}

/// "Search (⌘F)", with the platform's shortcut.
fn search_hint(t: crate::ui::i18n::Tr) -> String {
    let key = if cfg!(target_os = "macos") {
        "⌘F"
    } else {
        "Ctrl+F"
    };
    format!("{} ({key})", t.search())
}

fn checked(state: CheckState) -> bool {
    state == CheckState::On
}

// ---- page dispatch ----

pub enum PageView {
    None,
    Containers(Box<ContainersView>),
    Images(ImagesView),
    Volumes(VolumesView),
    Networks(NetworksView),
    Settings(SettingsView),
}

impl PageView {
    pub fn build(page: Page, root: WidgetId, collapsed: BTreeSet<String>, cx: &mut Ctx) -> Self {
        match page {
            Page::Containers => {
                PageView::Containers(Box::new(ContainersView::build(root, collapsed, cx)))
            }
            Page::Images => PageView::Images(ImagesView::build(root, cx)),
            Page::Volumes => PageView::Volumes(VolumesView::build(root, cx)),
            Page::Networks => PageView::Networks(NetworksView::build(root, cx)),
            Page::Settings => PageView::Settings(SettingsView::build(root, cx)),
        }
    }

    /// Ends the page's live streams.
    pub fn close(self, backend: &Backend) {
        if let PageView::Containers(mut v) = self {
            v.detail.stop(backend);
        }
    }

    pub fn refresh(&mut self, cx: &mut Ctx) {
        match self {
            PageView::None => {}
            PageView::Containers(v) => v.refresh(cx),
            PageView::Images(v) => v.refresh(cx),
            PageView::Volumes(v) => v.refresh(cx),
            PageView::Networks(v) => v.refresh(cx),
            PageView::Settings(v) => v.refresh(cx),
        }
    }

    pub fn handle(&mut self, m: &Message, cx: &mut Ctx) {
        match self {
            PageView::None => {}
            PageView::Containers(v) => v.handle(m, cx),
            PageView::Images(v) => v.handle(m, cx),
            PageView::Volumes(v) => v.handle(m, cx),
            PageView::Networks(v) => v.handle(m, cx),
            PageView::Settings(v) => v.handle(m, cx),
        }
    }

    pub fn shortcut(&mut self, shortcut: Shortcut, cx: &mut Ctx) {
        match self {
            PageView::Containers(v) => v.shortcut(shortcut, cx),
            PageView::Images(v) => v.shortcut(shortcut, cx),
            PageView::Volumes(v) => v.shortcut(shortcut, cx),
            PageView::Networks(v) => v.shortcut(shortcut, cx),
            PageView::None | PageView::Settings(_) => {}
        }
    }

    pub fn stream_event(&mut self, event: UiEvent, cx: &mut Ctx) {
        match (self, event) {
            (PageView::Containers(v), UiEvent::RunTemplate(result)) => {
                v.open_template(result, cx);
            }
            (PageView::Containers(v), UiEvent::UpdateChecks(result)) => {
                v.show_update_checks(result, cx);
            }
            (PageView::Containers(v), event) => v.detail.stream_event(event, cx),
            (PageView::Images(v), UiEvent::Inspect { id, result }) => {
                v.inspect.show(&id, result, cx);
            }
            (PageView::Volumes(v), UiEvent::Inspect { id, result }) => {
                v.inspect.show(&id, result, cx);
            }
            (PageView::Networks(v), UiEvent::Inspect { id, result }) => {
                v.inspect.show(&id, result, cx);
            }
            // A stream of a page that is gone: release it.
            (_, UiEvent::Logs(id, _) | UiEvent::Stats(id, _) | UiEvent::Exec(id, _)) => {
                commands::unsubscribe(cx.backend, id);
            }
            (_, UiEvent::ExecStarted { result: Ok(id), .. }) => {
                commands::unsubscribe(cx.backend, id);
            }
            _ => {}
        }
    }

    /// Opens the network map of stack `project` on the containers page (for
    /// screenshots), as its row's Map button does.
    #[cfg(test)]
    pub fn open_map(&mut self, project: &str, cx: &mut Ctx) {
        let PageView::Containers(v) = self else {
            return;
        };
        let row = v
            .rows
            .iter()
            .find(|r| matches!(r, Row::Group { project: p, .. } if p == project))
            .cloned();
        if let Some(row) = row {
            v.row_action(&row, 0, "map", cx);
        }
    }

    /// Selects the container `id` on the containers page (for screenshots).
    #[cfg(test)]
    pub fn select_container(&mut self, id: &str, cx: &mut Ctx) {
        let PageView::Containers(v) = self else {
            return;
        };
        let Some(row) = v
            .rows
            .iter()
            .position(|r| matches!(r, Row::Container { id: c, .. } if c == id))
        else {
            return;
        };
        let table = v.table.id;
        cx.tree
            .update::<TableView, _>(table, |t, ucx| t.select_rows(ucx, [row]));
        v.table.sync_selection(cx.tree);
        v.refresh(cx);
    }

    /// Opens the Exec tab of the container shown, the pane expanded or not (for
    /// screenshots).
    #[cfg(test)]
    pub fn open_exec(&mut self, expanded: bool, cx: &mut Ctx) {
        if let PageView::Containers(v) = self {
            v.detail.open_tab(Tab::Exec, cx);
            v.detail.set_expanded(expanded, cx);
        }
    }

    /// Passes a new exec terminal grid size on to the shell.
    pub fn sync_terminal_size(&mut self, tree: &mut UiTree, backend: &Backend) {
        if let PageView::Containers(v) = self {
            v.detail.sync_terminal_size(tree, backend);
        }
    }
}

// ---- containers ----

struct ComposeBox {
    root: WidgetId,
    title: WidgetId,
    text: WidgetId,
    cancel: WidgetId,
    close: WidgetId,
    project: Option<String>,
}

pub struct ContainersView {
    search: WidgetId,
    show_stopped: WidgetId,
    prune: WidgetId,
    /// Asks which containers have a newer image for their tag.
    check_updates: WidgetId,
    /// Updates every container the last check found a newer image for (shown then).
    update_all: WidgetId,
    bulk: BulkBar,
    /// Start / Stop on every selected container that can.
    start_selected: WidgetId,
    stop_selected: WidgetId,
    /// Says why the stack buttons are disabled when docker compose is missing.
    status: Note,
    empty: Note,
    table: Grid,
    compose: ComposeBox,
    detail: Detail,
    /// The Run form a container's Duplicate opens (in place of the detail pane).
    run_pane: RunPane,
    rows: Vec<Row>,
    filter: String,
    collapsed: BTreeSet<String>,
    /// The network map open over the window.
    map: Option<MapSheet>,
    /// The last check for newer images, by container id.
    updates: HashMap<String, Freshness>,
    /// Whether a check for newer images is running.
    checking: bool,
}

impl ContainersView {
    fn build(root: WidgetId, collapsed: BTreeSet<String>, cx: &mut Ctx) -> Self {
        let t = cx.t;
        let head = header(cx, root, t.nav_containers());
        let search = cx.tree.insert(
            Some(head),
            SearchField::new(search_hint(t), tokens::FIELD_SEARCH).capsule(false),
        );
        cx.tree.set_accessible_label(search, t.search_containers());
        cx.tree.insert(Some(head), widgets::spacer());
        let bulk = BulkBar::build(cx, head);
        let start_selected = button(cx.tree, head, t.start_selected(0));
        let stop_selected = button(cx.tree, head, t.stop_selected(0));
        let show_stopped = cx.tree.insert(
            Some(head),
            Checkbox::new(t.show_stopped()).checked(cx.config.show_stopped),
        );
        let update_all = button(cx.tree, head, t.update_all(0));
        cx.tree.set_hidden(update_all, true);
        let check_updates = button(cx.tree, head, t.check_updates());
        let prune = button(cx.tree, head, t.prune_stopped());
        let (body, root) = split(cx.tree, root);

        let status = Note::new(cx.tree, root);
        let empty = Note::new(cx.tree, root);
        let table = Grid::new(
            cx.tree,
            root,
            vec![
                TableColumn::new(t.col_state(), 108.0)
                    .font_family(widgets::MONO)
                    .sort(ColumnSort::Text),
                TableColumn::new(t.col_service_id(), 140.0)
                    .sort(ColumnSort::Text)
                    .detail_font_family(widgets::MONO)
                    .flex(1.3)
                    .shrink_to(90.0),
                TableColumn::new(t.col_image_ports(), 120.0)
                    .sort(ColumnSort::Text)
                    .shrink_to(90.0)
                    .font_family(widgets::MONO)
                    .flex(1.1),
                TableColumn::new(t.col_status(), 100.0)
                    .sort(ColumnSort::Text)
                    .flex(0.85)
                    .shrink_to(80.0),
                TableColumn::new(t.actions(), ACTIONS_WIDTH).align(ColumnAlign::Trailing),
            ],
        );

        let compose_root = fixed_pane(cx.tree, root, COMPOSE_HEIGHT);
        let bar = cx
            .tree
            .insert(Some(compose_root), widgets::row(tokens::SPACE_3));
        let compose_title = cx.tree.insert(
            Some(bar),
            Label::new("")
                .weight(tokens::WEIGHT_STRONG)
                .font_size(tokens::TEXT_BASE),
        );
        cx.tree.insert(Some(bar), widgets::spacer());
        let compose_cancel = button(cx.tree, bar, t.cancel());
        let compose_close = button(cx.tree, bar, t.close());
        let compose_text = cx.tree.insert(
            Some(compose_root),
            TextView::new(0.0).fill_height().read_only(true),
        );
        cx.tree
            .set_accessible_label(compose_text, t.compose_output());
        cx.tree.set_hidden(compose_root, true);

        let detail = Detail::build(body, root, cx);
        let run_pane = RunPane::build(body, cx);

        Self {
            search,
            show_stopped,
            prune,
            check_updates,
            update_all,
            bulk,
            start_selected,
            stop_selected,
            status,
            empty,
            table,
            compose: ComposeBox {
                root: compose_root,
                title: compose_title,
                text: compose_text,
                cancel: compose_cancel,
                close: compose_close,
                project: None,
            },
            detail,
            run_pane,
            rows: Vec::new(),
            filter: String::new(),
            collapsed,
            map: None,
            updates: HashMap::new(),
            checking: false,
        }
    }

    fn container<'a>(&self, cx: &'a Ctx, id: &str) -> Option<&'a Container> {
        cx.snap.containers.iter().find(|c| c.id == id)
    }

    /// The selected containers (stack rows are not removed in bulk).
    fn selected_containers(&self, cx: &Ctx) -> Vec<Container> {
        let keys = self.table.selected_keys(cx.tree);
        cx.snap
            .containers
            .iter()
            .filter(|c| keys.contains(&c.id))
            .cloned()
            .collect()
    }

    fn selected_row(&self) -> Option<&Row> {
        let key = self.table.selected.as_ref()?;
        self.rows.iter().find(|r| &r.key() == key)
    }

    fn refresh(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        self.rows = model::build_rows(
            &cx.snap.containers,
            &self.filter,
            cx.config.show_stopped,
            &self.collapsed,
        );
        let rows = self
            .rows
            .iter()
            .map(|r| container_grid_row(r, cx, &self.updates))
            .collect();
        self.table.set_rows(cx.tree, rows);

        let empty = if !self.rows.is_empty() {
            ""
        } else if cx.snap.containers.is_empty() {
            t.no_containers()
        } else {
            t.nothing_matches()
        };
        self.empty.set(cx, empty);
        cx.enable(self.prune, cx.connected());
        cx.enable(self.check_updates, cx.connected() && !self.checking);
        self.sync_update_all(cx);
        let n = self.selected_containers(cx).len();
        self.bulk.sync(cx, &self.table, n);
        self.sync_lifecycle_buttons(cx);
        self.refresh_actions(cx);
        self.refresh_compose(cx);
        let selected = match self.selected_row() {
            Some(Row::Container { id, .. }) => self.container(cx, id).cloned(),
            _ => None,
        };
        // The detail pane and the Run form share the side: a selection takes it back.
        if selected.is_some() {
            self.run_pane.hide(cx);
        }
        self.run_pane.sync(cx);
        cx.enable(self.detail.duplicate, cx.connected());
        cx.enable(self.detail.update, cx.connected());
        cx.enable(self.detail.networks, cx.connected());
        self.detail.forget_shells(cx);
        self.detail.set_container(selected.as_ref(), cx);
    }

    /// Asks for a Run form filled in from container `id`; it opens when it arrives.
    fn duplicate(&self, id: String, cx: &mut Ctx) {
        let backend = cx.backend.clone();
        cx.backend.spawn(async move {
            let result = commands::container_template(backend.clone(), id).await;
            backend.send(UiEvent::RunTemplate(result));
        });
    }

    /// Opens the Run form a Duplicate asked for, saying which settings it left out.
    fn open_template(&mut self, result: Result<RunTemplate, UiError>, cx: &mut Ctx) {
        let template = match result {
            Ok(template) => template,
            Err(e) => return cx.toast(e.message),
        };
        self.table.clear_selection(cx.tree);
        self.refresh(cx);
        self.run_pane.fill(&template, cx);
        if let Some(list) = model::skipped_settings(&template.skipped) {
            let t = cx.t;
            cx.toast(t.duplicate_skipped(&list));
        }
    }

    /// Asks which containers have a newer image; the rows say so when it arrives.
    fn check_for_updates(&mut self, cx: &mut Ctx) {
        self.checking = true;
        cx.enable(self.check_updates, false);
        let backend = cx.backend.clone();
        cx.backend.spawn(async move {
            let result = commands::check_updates(backend.clone()).await;
            backend.send(UiEvent::UpdateChecks(result));
        });
    }

    /// "Update all (n)…" is there while the last check found containers to update.
    fn sync_update_all(&self, cx: &mut Ctx) {
        let n = model::update_targets(&cx.snap.containers, &self.updates).len();
        let label = cx.t.update_all(n);
        cx.tree.update::<Button, _>(self.update_all, |b, ucx| {
            if b.label() != label {
                b.set_label(ucx, label);
            }
        });
        cx.show(self.update_all, n > 0);
        cx.enable(self.update_all, cx.connected());
    }

    /// Asks before updating every container the last check found a newer image for.
    fn ask_update_all(&self, cx: &mut Ctx) {
        let t = cx.t;
        let targets = model::update_targets(&cx.snap.containers, &self.updates);
        if targets.is_empty() {
            return;
        }
        let names: Vec<&str> = targets.iter().map(|(_, name)| name.as_str()).collect();
        cx.confirm(
            &t.update_all_title(targets.len()),
            &t.update_all_body(&names.join(", ")),
            t.act_update(),
            Act::UpdateContainers(targets.clone()),
        );
    }

    /// Marks the rows from a finished check and says how many can be updated.
    fn show_update_checks(&mut self, result: Result<Vec<UpdateCheck>, UiError>, cx: &mut Ctx) {
        let t = cx.t;
        self.checking = false;
        match result {
            Ok(checks) => {
                cx.toast(model::update_check_summary(&checks, t));
                self.updates = checks
                    .into_iter()
                    .map(|c| (c.container, c.freshness))
                    .collect();
            }
            Err(e) => cx.toast(e.message),
        }
        self.refresh(cx);
    }

    /// "Start selected (n)" and "Stop selected (n)" count the selected containers each
    /// can act on.
    fn sync_lifecycle_buttons(&self, cx: &mut Ctx) {
        let t = cx.t;
        let selected = self.selected_containers(cx);
        let start = selected.iter().filter(|c| model::startable(c)).count();
        let stop = selected.iter().filter(|c| model::stoppable(c)).count();
        let connected = cx.connected();
        for (id, label, n) in [
            (self.start_selected, t.start_selected(start), start),
            (self.stop_selected, t.stop_selected(stop), stop),
        ] {
            cx.tree.update::<Button, _>(id, |b, ucx| {
                if b.label() != label {
                    b.set_label(ucx, label);
                }
            });
            cx.enable(id, connected && n > 0);
        }
    }

    /// Starts (or stops) every selected container that can be.
    fn start_or_stop_selected(&self, start: bool, cx: &mut Ctx) {
        let (can, action): (fn(&Container) -> bool, _) = if start {
            (model::startable, ContainerAction::Start)
        } else {
            (model::stoppable, ContainerAction::Stop)
        };
        let ids: Vec<String> = self
            .selected_containers(cx)
            .into_iter()
            .filter(can)
            .map(|c| c.id)
            .collect();
        let r = commands::container_actions(cx.backend, ids, action);
        cx.attempt(r);
    }

    fn refresh_actions(&mut self, cx: &mut Ctx) {
        let stacks = self.rows.iter().any(|r| matches!(r, Row::Group { .. }));
        let notice = if stacks && cx.snap.compose_available == Some(false) {
            cx.t.compose_missing()
        } else {
            ""
        };
        self.status.set(cx, notice);
    }

    fn refresh_compose(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        let run = self
            .compose
            .project
            .as_ref()
            .and_then(|p| cx.snap.compose_runs.iter().find(|r| &r.project == p));
        let Some(run) = run else {
            cx.show(self.compose.root, false);
            return;
        };
        cx.show(self.compose.root, true);
        let status = match &run.result {
            RunResult::Running => t.compose_running().to_owned(),
            RunResult::Ok => t.compose_done().to_owned(),
            RunResult::Failed { message } => message.clone(),
        };
        let running = run.result == RunResult::Running;
        cx.set_label(
            self.compose.title,
            format!("{} — {} · {status}", run.command, run.project),
        );
        cx.show(self.compose.cancel, running);
        let body = run.lines.join("\n");
        cx.tree.update::<TextView, _>(self.compose.text, |v, ucx| {
            if v.text() != body {
                v.set_text(ucx, body);
            }
        });
    }

    fn handle(&mut self, m: &Message, cx: &mut Ctx) {
        let t = cx.t;
        let src = m.source;
        if src == self.search
            && let Some(TextChanged(s)) = m.downcast_ref::<TextChanged>()
        {
            self.filter = s.clone();
            self.refresh(cx);
            return;
        }
        if src == self.show_stopped
            && let Some(CheckboxChanged(state)) = m.downcast_ref::<CheckboxChanged>()
        {
            let mut config = cx.config.clone();
            config.show_stopped = checked(*state);
            *cx.save_config = Some(config);
            return;
        }
        if src == self.table.id {
            if m.downcast_ref::<RowSelectionChanged>().is_some() {
                self.table.sync_selection(cx.tree);
                self.refresh(cx);
            } else if let Some(click) = m.downcast_ref::<CellActionClicked>() {
                self.row_action_at(click.row, &click.action, cx);
            } else if let Some(click) = m.downcast_ref::<GroupActionClicked>() {
                self.row_action_at(click.row, &click.action, cx);
            } else if let Some(GroupRowClicked(row)) = m.downcast_ref::<GroupRowClicked>() {
                self.row_action_at(*row, "toggle", cx);
            }
            return;
        }
        if let Some(map) = &self.map
            && src == map.close
            && m.downcast_ref::<ButtonClicked>().is_some()
        {
            map.close(cx.tree);
            self.map = None;
            return;
        }
        if self.bulk.toggle(m, &mut self.table, cx.tree) {
            return self.refresh(cx);
        }
        if self.bulk.is_remove(m) {
            let containers = self.selected_containers(cx);
            return ask_remove_many(&containers, cx);
        }
        if (src == self.start_selected || src == self.stop_selected)
            && m.downcast_ref::<ButtonClicked>().is_some()
        {
            return self.start_or_stop_selected(src == self.start_selected, cx);
        }
        if src == self.check_updates && m.downcast_ref::<ButtonClicked>().is_some() {
            return self.check_for_updates(cx);
        }
        if src == self.update_all && m.downcast_ref::<ButtonClicked>().is_some() {
            return self.ask_update_all(cx);
        }
        if src == self.detail.close && m.downcast_ref::<ButtonClicked>().is_some() {
            self.table.clear_selection(cx.tree);
            self.refresh(cx);
            return;
        }
        if src == self.detail.duplicate && m.downcast_ref::<ButtonClicked>().is_some() {
            if let Some(id) = self.detail.container.clone() {
                self.duplicate(id, cx);
            }
            return;
        }
        if src == self.detail.update && m.downcast_ref::<ButtonClicked>().is_some() {
            let shown = self.detail.container.clone();
            if let Some(c) = shown.and_then(|id| self.container(cx, &id).cloned()) {
                ask_update(&c, cx);
            }
            return;
        }
        if src == self.detail.networks && m.downcast_ref::<ButtonClicked>().is_some() {
            let shown = self.detail.container.clone();
            if let Some(c) = shown.and_then(|id| self.container(cx, &id).cloned()) {
                ask_networks(&c, cx);
            }
            return;
        }
        if self.run_pane.handle(m, cx) {
            return;
        }
        if self.detail.handle(m, cx) {
            return;
        }
        if m.downcast_ref::<ButtonClicked>().is_none() {
            return;
        }
        if src == self.prune {
            let stopped = cx
                .snap
                .containers
                .iter()
                .filter(|c| !model::is_running(c) && c.state != ContainerState::Paused)
                .count();
            if stopped == 0 {
                cx.toast(t.no_stopped());
            } else {
                cx.confirm(
                    t.prune_stopped_title(),
                    &t.prune_stopped_body(stopped),
                    t.prune(),
                    Act::Prune(PruneKind::Containers),
                );
            }
            return;
        }
        if src == self.compose.close {
            if let Some(p) = self.compose.project.take()
                && cx
                    .snap
                    .compose_runs
                    .iter()
                    .any(|r| r.project == p && r.result != RunResult::Running)
            {
                commands::dismiss_compose(cx.backend, &p);
            }
            cx.show(self.compose.root, false);
            return;
        }
        if src == self.compose.cancel
            && let Some(p) = &self.compose.project
        {
            let r = commands::cancel_compose(cx.backend, p);
            cx.attempt(r);
        }
    }

    fn row_action_at(&mut self, index: usize, action: &str, cx: &mut Ctx) {
        if let Some(key) = self.table.key_at(index).map(str::to_owned)
            && let Some(row) = self.rows.iter().find(|r| r.key() == key).cloned()
        {
            self.row_action(&row, index, action, cx);
        }
    }

    /// A button in a row of the table: `action` names it (see [`container_grid_row`]).
    fn row_action(&mut self, row: &Row, index: usize, action: &str, cx: &mut Ctx) {
        let t = cx.t;
        match row {
            Row::Container { id, .. } => {
                let Some(c) = self.container(cx, id).cloned() else {
                    return;
                };
                let simple = match action {
                    "start" => Some(ContainerAction::Start),
                    "stop" => Some(ContainerAction::Stop),
                    "pause" => Some(ContainerAction::Pause),
                    "resume" => Some(ContainerAction::Unpause),
                    "restart" => Some(ContainerAction::Restart),
                    _ => None,
                };
                if let Some(action) = simple {
                    let r = commands::container_action(cx.backend, c.id, action);
                    cx.attempt(r);
                    return;
                }
                match action {
                    "kill" => cx.confirm(
                        &t.kill_title(&c.name),
                        t.kill_body(),
                        t.act_kill(),
                        Act::Container(c.id.clone(), ContainerAction::Kill),
                    ),
                    "remove" => ask_remove(&c, cx),
                    "logs" | "exec" => {
                        cx.tree.update::<TableView, _>(self.table.id, |t, ucx| {
                            t.select_rows(ucx, [index])
                        });
                        self.table.sync_selection(cx.tree);
                        self.refresh(cx);
                        let tab = if action == "logs" {
                            Tab::Logs
                        } else {
                            Tab::Exec
                        };
                        self.detail.open_tab(tab, cx);
                    }
                    _ => {}
                }
            }
            Row::Group { project, .. } => {
                let cmd = match action {
                    "up" => ComposeCommand::Up,
                    "start" => ComposeCommand::Start,
                    "stop" => ComposeCommand::Stop,
                    "restart" => ComposeCommand::Restart,
                    "pull" => ComposeCommand::Pull,
                    "down" => {
                        self.compose.project = Some(project.clone());
                        return cx.confirm(
                            &t.compose_down_title(project),
                            t.compose_down_body(),
                            "Down",
                            Act::Compose(project.clone(), ComposeCommand::Down),
                        );
                    }
                    "toggle" => {
                        if !self.collapsed.remove(project) {
                            self.collapsed.insert(project.clone());
                        }
                        return self.refresh(cx);
                    }
                    "map" => {
                        let map =
                            stack_map::stack_map(project, &cx.snap.containers, &cx.snap.networks);
                        self.map = Some(MapSheet::open(cx.tree, t, project, &map));
                        return;
                    }
                    _ => return,
                };
                self.compose.project = Some(project.clone());
                let r = commands::compose(cx.backend, project, cmd);
                cx.attempt(r);
            }
        }
    }

    fn shortcut(&mut self, shortcut: Shortcut, cx: &mut Ctx) {
        let container = matches!(self.selected_row(), Some(Row::Container { .. }));
        if shortcut == Shortcut::command('f') {
            cx.tree.set_focus(Some(self.search));
        } else if shortcut == Shortcut::command('l') && container {
            self.detail.open_tab(Tab::Logs, cx);
        } else if shortcut == Shortcut::command('e') && container {
            self.detail.open_tab(Tab::Exec, cx);
        } else if shortcut == Shortcut::key(Key::Escape) {
            self.table.clear_selection(cx.tree);
            self.run_pane.hide(cx);
            self.refresh(cx);
        } else if shortcut == Shortcut::key(Key::Delete)
            && cx.connected()
            && let Some(Row::Container { id, .. }) = self.selected_row().cloned()
            && let Some(c) = self.container(cx, &id).cloned()
        {
            ask_remove(&c, cx);
        }
    }
}

/// Confirms, then removes `c` with its anonymous volumes (stopping it first if needed).
/// Asks before updating `c` to the newest image of its tag, or says how a stack's
/// container is updated instead.
fn ask_update(c: &Container, cx: &mut Ctx) {
    let t = cx.t;
    if !model::updatable(c) {
        return cx.toast(t.update_compose_hint());
    }
    cx.confirm(
        &t.update_title(&c.name),
        t.update_body(),
        t.act_update(),
        Act::UpdateContainer(c.id.clone(), c.name.clone()),
    );
}

/// Offers `c`'s network changes: a button for each network it can join or leave.
fn ask_networks(c: &Container, cx: &mut Ctx) {
    let t = cx.t;
    let choices: Vec<(String, Act)> = model::network_choices(c, &cx.snap.networks)
        .into_iter()
        .map(|choice| match choice {
            model::NetworkChoice::Connect(n) => (
                t.connect_to(&n),
                Act::ContainerNetwork(c.id.clone(), n, true),
            ),
            model::NetworkChoice::Disconnect(n) => (
                t.disconnect_from(&n),
                Act::ContainerNetwork(c.id.clone(), n, false),
            ),
        })
        .collect();
    if choices.is_empty() {
        return cx.toast(t.no_network_choices(&c.name));
    }
    cx.choose(
        &t.networks_title(&c.name),
        &t.networks_body(&c.networks.join(", ")),
        choices,
    );
}

fn ask_remove(c: &Container, cx: &mut Ctx) {
    let t = cx.t;
    let running = model::is_running(c);
    let body = if running {
        t.remove_running_body(&c.name)
    } else {
        t.remove_stopped_body(&c.name)
    };
    cx.confirm(
        &t.remove_container_title(&c.name),
        &body,
        t.remove(),
        Act::Container(
            c.id.clone(),
            ContainerAction::Remove {
                force: running,
                volumes: true,
            },
        ),
    );
}

/// Confirms, then removes every container in `cs` like [`ask_remove`] does one.
fn ask_remove_many(cs: &[Container], cx: &mut Ctx) {
    let t = cx.t;
    if cs.is_empty() {
        return;
    }
    let running = cs.iter().filter(|c| model::is_running(c)).count();
    cx.confirm(
        &t.remove_containers_title(cs.len()),
        &t.remove_containers_body(running),
        t.remove(),
        Act::RemoveContainers(
            cs.iter()
                .map(|c| (c.id.clone(), model::is_running(c)))
                .collect(),
        ),
    );
}

/// A row of the containers table, its last cell the row's buttons: a container's
/// actions for its state, or a stack's compose commands.
/// `updates` is the last check for newer images, by container id.
fn container_grid_row(row: &Row, cx: &Ctx, updates: &HashMap<String, Freshness>) -> GridRow {
    let t = cx.t;
    let i = icons();
    let connected = cx.connected();
    let button =
        |action: &'static str, icon: &VectorIcon| row_button(t, action, icon).enabled(connected);
    const ACTIONS: usize = 4;
    let stack_busy = |project: &str| {
        !connected
            || cx.snap.compose_available != Some(true)
            || cx.snap.pending.contains(&format!("compose:{project}"))
    };
    let compose = |action: &'static str, label: &str, busy: bool| {
        CellButton::new(action)
            .label(label)
            .height(GROUP_BUTTON)
            .enabled(!busy)
    };
    match row {
        Row::Group {
            project,
            running,
            total,
            collapsed,
        } => {
            let busy = stack_busy(project);
            let buttons = vec![
                CellButton::new("map")
                    .label(t.network_map())
                    .height(GROUP_BUTTON),
                compose("up", "Up", busy),
                compose("start", "Start", busy),
                compose("restart", "Restart", busy),
                compose("stop", "Stop", busy),
                compose("pull", "Pull", busy),
                compose("down", "Down", busy),
            ];
            let tone = if *running == *total {
                Tone::Success
            } else if *running > 0 {
                Tone::Warning
            } else {
                Tone::Neutral
            };
            GridRow::new(
                row.key(),
                vec![
                    if *collapsed { "▸" } else { "▾" }.into(),
                    project.clone(),
                    String::new(),
                    String::new(),
                    String::new(),
                ],
            )
            .note(1, t.running_count(*running, *total))
            .dot(1, tone)
            .group(buttons)
        }
        Row::Container { id, .. } => {
            let Some(c) = cx.snap.containers.iter().find(|c| &c.id == id) else {
                return GridRow::new(row.key(), vec![String::new(); 5]);
            };
            let name = match &c.compose {
                Some(l) => format!("{} ({})", l.service, c.name),
                None => c.name.clone(),
            };
            let busy = cx.snap.pending.contains(&c.id);
            let running = model::is_running(c);
            let mut buttons = if c.state == ContainerState::Paused {
                vec![button("resume", &i.play), button("stop", &i.stop)]
            } else if running {
                vec![
                    button("stop", &i.stop),
                    button("pause", &i.pause),
                    button("kill", &i.kill),
                    button("restart", &i.restart),
                ]
            } else {
                vec![button("start", &i.play), button("restart", &i.restart)]
            };
            if busy {
                buttons = buttons.into_iter().map(|b| b.enabled(false)).collect();
            }
            buttons.push(row_button(t, "logs", &i.logs));
            buttons.push(button("exec", &i.exec).enabled(connected && running));
            buttons.push(button("remove", &i.trash).enabled(connected && !busy));
            GridRow::new(
                row.key(),
                vec![
                    model::state_label(c.state).to_owned(),
                    name,
                    c.image.clone(),
                    c.status.clone(),
                    String::new(),
                ],
            )
            .look(0, CellLook::StrongOutlinedBadge(state_tone(c.state)))
            .detail(1, &c.id[..c.id.len().min(12)])
            .detail(2, c.ports.join(", "))
            .detail(3, model::freshness_note(updates.get(&c.id), t))
            .buttons(ACTIONS, buttons)
        }
    }
}

// ---- container detail ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Logs,
    Inspect,
    Stats,
    Exec,
}

const TABS: [Tab; 4] = [Tab::Logs, Tab::Inspect, Tab::Stats, Tab::Exec];

struct Detail {
    root: WidgetId,
    title: WidgetId,
    /// The container's state, in its color.
    state: WidgetId,
    /// Opens the Run form filled in with this container's settings.
    duplicate: WidgetId,
    /// Replaces the container with one made from the newest image of its tag.
    update: WidgetId,
    /// Connects the container to a network, or disconnects it from one.
    networks: WidgetId,
    close: WidgetId,
    tabs: WidgetId,
    body: WidgetId,
    tab: Tab,
    container: Option<String>,
    running: bool,
    /// Whether the engine was connected at the last update.
    connected: bool,
    // Logs
    logs: LogBuffer,
    /// How many of the buffer's finished lines the view has.
    logs_shown: u64,
    log_sub: Option<u32>,
    log_search: Option<WidgetId>,
    copy_logs: Option<WidgetId>,
    clear: Option<WidgetId>,
    count: Option<WidgetId>,
    log_view: Option<WidgetId>,
    // Inspect
    inspect_json: Option<String>,
    secrets: Option<WidgetId>,
    copy_redacted: Option<WidgetId>,
    copy_raw: Option<WidgetId>,
    inspect_text: Option<WidgetId>,
    // Stats
    stats_sub: Option<u32>,
    stats: Vec<WidgetId>,
    // Exec
    exec: ExecPane,
    /// The list beside the pane, hidden while the pane takes the whole width.
    list: WidgetId,
    expand: WidgetId,
    expanded: bool,
}

/// The Exec tab: the fields a shell starts with, and a terminal per container whose
/// shell keeps running while another tab or container is shown.
struct ExecPane {
    root: WidgetId,
    command: WidgetId,
    user: WidgetId,
    working_dir: WidgetId,
    /// Starts a new shell: after the last one ended, or in place of the running one.
    open: WidgetId,
    status: WidgetId,
    terminals: WidgetId,
    /// By container id.
    shells: HashMap<String, Shell>,
}

struct Shell {
    terminal: WidgetId,
    sub: Option<u32>,
    starting: bool,
    /// Why it is not running (it exited or failed to start).
    ended: Option<String>,
}

impl ExecPane {
    fn build(parent: WidgetId, cx: &mut Ctx) -> Self {
        let t = cx.t;
        let root = cx
            .tree
            .insert(Some(parent), widgets::fill_column(tokens::SPACE_2, 0.0));
        let row = cx
            .tree
            .insert(Some(root), widgets::wrap_row(tokens::SPACE_2));
        let field = |cx: &mut Ctx, hint: &str, name: &str, width: f32| {
            let id = cx.tree.insert(Some(row), TextField::new(hint, width));
            cx.tree.set_accessible_label(id, name);
            id
        };
        let command = field(
            cx,
            t.exec_command_hint(),
            t.exec_command(),
            tokens::FIELD_WIDE,
        );
        let user = field(cx, t.exec_user(), t.exec_user(), tokens::FIELD_NUMBER);
        let working_dir = field(
            cx,
            t.exec_working_dir(),
            t.exec_working_dir(),
            tokens::FIELD_NAME,
        );
        let open = button(cx.tree, row, t.exec_new_shell());
        let status = muted(cx.tree, root, "");
        let terminals = cx.tree.insert(Some(root), widgets::fill_column(0.0, 0.0));
        cx.tree.set_hidden(root, true);
        Self {
            root,
            command,
            user,
            working_dir,
            open,
            status,
            terminals,
            shells: HashMap::new(),
        }
    }

    fn fields(&self, tree: &UiTree) -> ExecShell {
        let text = |id| {
            tree.get::<TextField>(id)
                .map(|f| f.text().to_owned())
                .unwrap_or_default()
        };
        ExecShell {
            command: text(self.command),
            user: text(self.user),
            working_dir: text(self.working_dir),
        }
    }

    /// The shell whose stream is `sub`.
    fn by_sub(&mut self, sub: u32) -> Option<&mut Shell> {
        self.shells.values_mut().find(|s| s.sub == Some(sub))
    }

    fn drop_shell(shell: Shell, cx: &mut Ctx) {
        if let Some(sub) = shell.sub {
            commands::unsubscribe(cx.backend, sub);
        }
        cx.tree.remove(shell.terminal, cx.text);
    }
}

impl Detail {
    fn build(parent: WidgetId, list: WidgetId, cx: &mut Ctx) -> Self {
        let t = cx.t;
        let (root, inner) = side_pane(cx.tree, parent);
        // Tabs and Close at the top; the container's name, state and actions below.
        let top = cx
            .tree
            .insert(Some(inner), widgets::wrap_row(tokens::SPACE_3));
        let tabs = cx.tree.insert(
            Some(top),
            SegmentedControl::new(
                [t.tab_logs(), t.tab_inspect(), t.tab_stats(), t.tab_exec()],
                SegmentMode::SelectOne,
            )
            .style(SegmentStyle::Underline)
            .with_selected(&[0]),
        );
        cx.tree.insert(Some(top), widgets::spacer());
        let expand = cx.tree.insert(
            Some(top),
            Button::new(t.expand_pane())
                .style(noa_gpui_platform::noa_gpui_core::widgets::ButtonStyle::Recessed),
        );
        let close = cx.tree.insert(
            Some(top),
            Button::new(t.close())
                .style(noa_gpui_platform::noa_gpui_core::widgets::ButtonStyle::Recessed),
        );
        let bar = cx
            .tree
            .insert(Some(inner), widgets::wrap_row(tokens::SPACE_3));
        let title = cx.tree.insert(
            Some(bar),
            Label::new("")
                .weight(tokens::WEIGHT_STRONG)
                .font_size(tokens::TEXT_BASE)
                .font_family(widgets::MONO),
        );
        let state = cx.tree.insert(
            Some(bar),
            Tag::new("")
                .style(TagStyle::StrongOutlined)
                .font_family(widgets::MONO),
        );
        cx.tree.insert(Some(bar), widgets::spacer());
        let duplicate = cx.tree.insert(
            Some(bar),
            Button::new(t.duplicate())
                .style(noa_gpui_platform::noa_gpui_core::widgets::ButtonStyle::Recessed),
        );
        let update = cx.tree.insert(
            Some(bar),
            Button::new(t.update_container())
                .style(noa_gpui_platform::noa_gpui_core::widgets::ButtonStyle::Recessed),
        );
        let networks = cx.tree.insert(
            Some(bar),
            Button::new(t.container_networks())
                .style(noa_gpui_platform::noa_gpui_core::widgets::ButtonStyle::Recessed),
        );
        cx.tree
            .insert(Some(inner), widgets::rule(tokens::RULE, Role::Separator));
        let body = cx
            .tree
            .insert(Some(inner), widgets::fill_column(tokens::SPACE_2, 0.0));
        let exec = ExecPane::build(inner, cx);
        Self {
            root,
            title,
            state,
            duplicate,
            update,
            networks,
            close,
            tabs,
            body,
            tab: Tab::Logs,
            container: None,
            running: false,
            connected: cx.connected(),
            logs: LogBuffer::new(cx.config.log_lines as usize),
            logs_shown: 0,
            log_sub: None,
            log_search: None,
            copy_logs: None,
            clear: None,
            count: None,
            log_view: None,
            inspect_json: None,
            secrets: None,
            copy_redacted: None,
            copy_raw: None,
            inspect_text: None,
            stats_sub: None,
            stats: Vec::new(),
            exec,
            list,
            expand,
            expanded: false,
        }
    }

    /// Ends the logs and stats streams; the shells keep running.
    fn stop_streams(&mut self, backend: &Backend) {
        for sub in [self.log_sub.take(), self.stats_sub.take()]
            .into_iter()
            .flatten()
        {
            commands::unsubscribe(backend, sub);
        }
    }

    /// Ends every stream, the shells' too.
    fn stop(&mut self, backend: &Backend) {
        self.stop_streams(backend);
        for shell in self.exec.shells.values_mut() {
            if let Some(sub) = shell.sub.take() {
                commands::unsubscribe(backend, sub);
            }
        }
    }

    /// Lets the pane take the list's width too, or gives it back.
    fn set_expanded(&mut self, expanded: bool, cx: &mut Ctx) {
        if self.expanded == expanded {
            return;
        }
        self.expanded = expanded;
        cx.show(self.list, !expanded);
        let share = if expanded { 1.0 } else { tokens::PANE_SHARE };
        cx.tree.update::<Boxed, _>(self.root, |c, ucx| {
            c.set_style(ucx, widgets::side_pane_style(share, tokens::PANE_MIN))
        });
        let label = if expanded {
            cx.t.restore_pane()
        } else {
            cx.t.expand_pane()
        };
        cx.tree
            .update::<Button, _>(self.expand, |b, ucx| b.set_label(ucx, label));
    }

    /// Forgets the shells of containers that are gone.
    fn forget_shells(&mut self, cx: &mut Ctx) {
        let snap = cx.snap;
        let gone: Vec<String> = self
            .exec
            .shells
            .keys()
            .filter(|id| !snap.containers.iter().any(|c| &c.id == *id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(shell) = self.exec.shells.remove(&id) {
                ExecPane::drop_shell(shell, cx);
            }
        }
    }

    /// Shows the shell of the container shown, starting one when it has none (or,
    /// with `restart_ended`, when its last one ended).
    fn show_shell(&mut self, restart_ended: bool, cx: &mut Ctx) {
        let Some(id) = self.container.clone() else {
            return;
        };
        let start = match self.exec.shells.get(&id) {
            None => true,
            Some(s) => restart_ended && s.ended.is_some(),
        };
        if start {
            self.start_shell(cx);
        }
        self.sync_exec(cx);
        if let Some(s) = self.exec.shells.get(&id)
            && s.sub.is_some()
            && self.tab == Tab::Exec
        {
            cx.tree.set_focus(Some(s.terminal));
        }
    }

    /// Starts a shell for the container shown with the fields as typed, in place of
    /// the one it has.
    fn start_shell(&mut self, cx: &mut Ctx) {
        let Some(id) = self.container.clone() else {
            return;
        };
        if !self.running || !cx.connected() {
            return;
        }
        let shell = self.exec.fields(cx.tree);
        if let Err(e) = run_spec::parse_exec(&shell) {
            return cx.toast(e.message);
        }
        match self.exec.shells.remove(&id) {
            // One start at a time per container.
            Some(old) if old.starting => {
                self.exec.shells.insert(id, old);
                return;
            }
            Some(old) => ExecPane::drop_shell(old, cx),
            None => {}
        }
        let terminal = cx.tree.insert(
            Some(self.exec.terminals),
            Terminal::new().font_size(tokens::TEXT_SM).scrollback(5_000),
        );
        self.exec.shells.insert(
            id.clone(),
            Shell {
                terminal,
                sub: None,
                starting: true,
                ended: None,
            },
        );
        let backend = cx.backend.clone();
        cx.backend.spawn(async move {
            // The real grid size follows once the session runs (see
            // `sync_terminal_size`).
            let result = commands::exec_start(backend.clone(), id.clone(), shell, 80, 24).await;
            backend.send(UiEvent::ExecStarted {
                container: id,
                result,
            });
        });
        self.sync_exec(cx);
    }

    /// Shows the terminal of the container shown (and no other), and what its shell
    /// is doing.
    fn sync_exec(&self, cx: &mut Ctx) {
        let t = cx.t;
        let current = self.container.as_deref();
        for (id, s) in &self.exec.shells {
            cx.show(s.terminal, Some(id.as_str()) == current);
        }
        let can_start = self.running && cx.connected();
        let shell = current.and_then(|id| self.exec.shells.get(id));
        let (status, open, enabled) = match shell {
            Some(s) if s.starting => (
                Some(t.starting_shell().to_owned()),
                t.exec_new_shell(),
                false,
            ),
            Some(s) if s.sub.is_some() => (None, t.exec_new_shell(), can_start),
            _ if !can_start => (
                Some(t.start_to_exec().to_owned()),
                t.exec_reconnect(),
                false,
            ),
            Some(s) => (s.ended.clone(), t.exec_reconnect(), true),
            None => (None, t.exec_new_shell(), true),
        };
        cx.show(self.exec.status, status.is_some());
        cx.set_label(self.exec.status, status.unwrap_or_default());
        cx.tree
            .update::<Button, _>(self.exec.open, |b, ucx| b.set_label(ucx, open));
        cx.enable(self.exec.open, enabled);
    }

    /// Shows `c` (or hides the pane), restarting the tab when the container changed,
    /// when the connection came back (the engine's streams end when it drops), and
    /// when the container's state changed what the tab can show.
    fn set_container(&mut self, c: Option<&Container>, cx: &mut Ctx) {
        let id = c.map(|c| c.id.clone());
        let running = c.is_some_and(model::is_running);
        let connected = cx.connected();
        let reconnected = connected && !self.connected;
        self.connected = connected;
        if id == self.container {
            self.show_state(c, cx);
            let started = running && !self.running;
            let changed = running != self.running;
            self.running = running;
            if reconnected {
                // The engine's streams ended with the connection.
                for shell in self.exec.shells.values_mut() {
                    if let Some(sub) = shell.sub.take() {
                        commands::unsubscribe(cx.backend, sub);
                        shell.ended = Some(cx.t.exec_exited().to_owned());
                    }
                }
            }
            if self.tab == Tab::Exec && (changed || reconnected) {
                // A shell again once the container runs again.
                self.show_shell(started || reconnected, cx);
            }
            let restart = match self.tab {
                // Stats follow the container's state.
                Tab::Stats => changed || reconnected,
                Tab::Exec => false,
                // A stopped container's logs end; follow them again once it runs.
                Tab::Logs => reconnected || (started && self.log_sub.is_none()),
                Tab::Inspect => false,
            };
            if restart && id.is_some() {
                self.open_tab(self.tab, cx);
            }
            return;
        }
        self.container = id;
        self.running = running;
        cx.show(self.root, c.is_some());
        if let Some(c) = c {
            cx.set_label(self.title, c.name.clone());
            self.open_tab(self.tab, cx);
        } else {
            self.stop_streams(cx.backend);
            self.clear_body(cx);
            self.set_expanded(false, cx);
        }
        self.show_state(c, cx);
    }

    fn show_state(&self, c: Option<&Container>, cx: &mut Ctx) {
        let Some(c) = c else {
            return;
        };
        let (text, tone) = (model::state_label(c.state), state_tone(c.state));
        cx.tree.update::<Tag, _>(self.state, |tag, ucx| {
            if tag.text() != text {
                tag.set_text(ucx, text);
            }
            tag.set_tone(ucx, tone);
        });
    }

    fn clear_body(&mut self, cx: &mut Ctx) {
        for child in cx.tree.children(self.body).to_vec() {
            cx.tree.remove(child, cx.text);
        }
        self.log_search = None;
        self.copy_logs = None;
        self.clear = None;
        self.count = None;
        self.log_view = None;
        self.secrets = None;
        self.copy_redacted = None;
        self.copy_raw = None;
        self.inspect_text = None;
        self.stats.clear();
    }

    fn open_tab(&mut self, tab: Tab, cx: &mut Ctx) {
        let t = cx.t;
        self.stop_streams(cx.backend);
        self.clear_body(cx);
        self.tab = tab;
        cx.show(self.body, tab != Tab::Exec);
        cx.show(self.exec.root, tab == Tab::Exec);
        let index = TABS.iter().position(|x| *x == tab).unwrap_or(0);
        cx.tree
            .update::<SegmentedControl, _>(self.tabs, |s, ucx| s.set_selected(ucx, &[index]));
        let Some(id) = self.container.clone() else {
            return;
        };
        let body = self.body;
        match tab {
            Tab::Logs => {
                self.logs = LogBuffer::new(cx.config.log_lines as usize);
                self.logs_shown = 0;
                // Search, the count, Copy and Clear over the lines; the view
                // follows new lines while scrolled to the bottom.
                let bar = cx.tree.insert(Some(body), widgets::row(tokens::SPACE_3));
                let search = cx.tree.insert(
                    Some(bar),
                    SearchField::new(t.search_logs(), tokens::FIELD_WIDE).capsule(false),
                );
                cx.tree.set_accessible_label(search, t.search_logs_name());
                self.log_search = Some(search);
                self.count = Some(muted(cx.tree, bar, t.line_count(0)));
                cx.tree.insert(Some(bar), widgets::spacer());
                self.copy_logs = Some(button(cx.tree, bar, t.copy()));
                self.clear = Some(button(cx.tree, bar, t.clear()));
                self.log_view = Some(
                    cx.tree.insert(
                        Some(body),
                        LogView::new()
                            .max_lines(cx.config.log_lines as usize)
                            .momentum(false)
                            .font_family(widgets::MONO)
                            .font_size(tokens::TEXT_SM)
                            .fill_height(),
                    ),
                );
                match commands::subscribe_logs(cx.backend, id) {
                    Ok(sub) => self.log_sub = Some(sub),
                    Err(e) => cx.toast(e.message),
                }
            }
            Tab::Inspect => {
                let bar = cx.tree.insert(Some(body), widgets::row(tokens::SPACE_3));
                self.secrets = Some(cx.tree.insert(Some(bar), Checkbox::new(t.show_secrets())));
                self.copy_redacted = Some(button(cx.tree, bar, t.copy_redacted()));
                self.copy_raw = Some(button(cx.tree, bar, t.copy_raw()));
                let text = cx.tree.insert(
                    Some(body),
                    TextView::new(0.0)
                        .fill_height()
                        .read_only(true)
                        .placeholder(t.loading()),
                );
                cx.tree.set_accessible_label(text, t.inspect_output());
                self.inspect_text = Some(text);
                self.inspect_json = None;
                let backend = cx.backend.clone();
                cx.backend.spawn(async move {
                    let result =
                        commands::inspect(backend.clone(), InspectKind::Container, id.clone())
                            .await;
                    backend.send(UiEvent::Inspect { id, result });
                });
            }
            Tab::Stats => {
                let grid = cx.tree.insert(Some(body), widgets::column(tokens::SPACE_3));
                for name in [
                    t.stat_cpu(),
                    t.stat_memory(),
                    t.stat_network(),
                    t.stat_pids(),
                ] {
                    let row = cx.tree.insert(Some(grid), widgets::row(tokens::SPACE_4));
                    cx.tree.insert(
                        Some(row),
                        Label::new(name)
                            .weight(tokens::WEIGHT_STRONG)
                            .font_size(tokens::TEXT_BASE),
                    );
                    self.stats.push(label(cx.tree, row, "—"));
                }
                let note = muted(cx.tree, grid, t.waiting_stats());
                self.stats.push(note);
                if self.running {
                    match commands::subscribe_stats(cx.backend, id) {
                        Ok(sub) => self.stats_sub = Some(sub),
                        Err(e) => cx.toast(e.message),
                    }
                } else {
                    cx.set_label(note, t.stats_ended());
                }
            }
            Tab::Exec => self.show_shell(false, cx),
        }
    }

    /// Tells the shell the terminal's grid size when it changed.
    fn sync_terminal_size(&mut self, tree: &mut UiTree, backend: &Backend) {
        let Some(shell) = self
            .container
            .as_ref()
            .and_then(|id| self.exec.shells.get(id))
        else {
            return;
        };
        let (term, Some(sub)) = (shell.terminal, shell.sub) else {
            return;
        };
        if let Some(Some((cols, rows))) = tree.update::<Terminal, _>(term, |t, _| t.take_resized())
        {
            let _ = commands::exec_resize(backend, sub, cols as u16, rows as u16);
        }
    }

    /// Adds the lines finished since the last call to the view: appending (rather than
    /// replacing them all) keeps its scroll position and selection, and it follows
    /// the new lines only while scrolled to the bottom.
    fn refresh_logs(&mut self, cx: &mut Ctx) {
        let (Some(view), Some(count)) = (self.log_view, self.count) else {
            return;
        };
        let theme = cx.tree.theme();
        let (stderr, console) = (theme.critical, theme.muted);
        let lines: Vec<LogLine> = self
            .logs
            .since(self.logs_shown)
            .map(|(stream, text)| {
                let line = LogLine::new(text.clone());
                match stream {
                    LogStream::Stderr => line.color(stderr),
                    LogStream::Console => line.color(console),
                    _ => line,
                }
            })
            .collect();
        self.logs_shown = self.logs.finished();
        let mut n = 0;
        cx.tree.update::<LogView, _>(view, |v, ucx| {
            v.extend(ucx, lines);
            n = v.line_count();
        });
        cx.set_label(count, cx.t.line_count(n));
    }

    /// Highlights the lines matching `action`'s search and shows how many there are.
    fn find_in_logs(&mut self, action: FindAction, cx: &mut Ctx) {
        let (Some(view), Some(count)) = (self.log_view, self.count) else {
            return;
        };
        let mut status = FindStatus::default();
        cx.tree
            .update::<LogView, _>(view, |v, ucx| status = v.find(ucx, &action));
        let query_empty = matches!(&action, FindAction::Search { query, .. } if query.is_empty());
        let text = if query_empty {
            cx.t.line_count(self.logs.lines().len())
        } else {
            cx.t.match_count(status.current.map_or(0, |i| i + 1), status.total)
        };
        cx.set_label(count, text);
    }

    fn show_inspect(&mut self, cx: &mut Ctx) {
        let (Some(json), Some(view)) = (&self.inspect_json, self.inspect_text) else {
            return;
        };
        let reveal = self
            .secrets
            .and_then(|s| cx.tree.get::<Checkbox>(s))
            .is_some_and(|c| checked(c.state()));
        let shown = if reveal {
            json.clone()
        } else {
            model::redact_inspect(json)
        };
        cx.tree
            .update::<TextView, _>(view, |v, ucx| v.set_text(ucx, shown));
    }

    /// Shows output in the terminal and writes back what the terminal answers.
    fn feed_terminal(term: WidgetId, sub: u32, bytes: &[u8], cx: &mut Ctx) {
        let replies = cx
            .tree
            .update::<Terminal, _>(term, |t, ucx| t.feed(ucx, bytes))
            .unwrap_or_default();
        if !replies.is_empty() {
            let _ = commands::exec_input(cx.backend, sub, replies);
        }
    }

    fn stream_event(&mut self, event: UiEvent, cx: &mut Ctx) {
        let t = cx.t;
        match event {
            UiEvent::Logs(sub, batch) => {
                if Some(sub) != self.log_sub {
                    return commands::unsubscribe(cx.backend, sub);
                }
                if batch.ended.is_none() && !batch.chunks.is_empty() {
                    commands::ack_stream(cx.backend, sub, 1);
                }
                for chunk in &batch.chunks {
                    self.logs.push(chunk.stream, &chunk.text);
                }
                if let Some(end) = batch.ended {
                    self.logs
                        .push(LogStream::Console, &format!("\n— {end} —\n"));
                    self.log_sub = None;
                }
                self.refresh_logs(cx);
            }
            UiEvent::Stats(sub, ev) => {
                if Some(sub) != self.stats_sub {
                    return commands::unsubscribe(cx.backend, sub);
                }
                let [cpu, mem, net, pids, note] = self.stats[..] else {
                    return;
                };
                match ev {
                    StatsEvent::Sample(s) => {
                        let pct = if s.mem_limit > 0.0 {
                            format!(" ({:.1}%)", s.mem_usage / s.mem_limit * 100.0)
                        } else {
                            String::new()
                        };
                        cx.set_label(cpu, format!("{:.1}%", s.cpu_percent));
                        cx.set_label(
                            mem,
                            format!(
                                "{} / {}{pct}",
                                model::format_bytes(s.mem_usage),
                                model::format_bytes(s.mem_limit)
                            ),
                        );
                        cx.set_label(
                            net,
                            format!(
                                "↓ {}  ↑ {}",
                                model::format_bytes(s.net_rx),
                                model::format_bytes(s.net_tx)
                            ),
                        );
                        cx.set_label(pids, format!("{}", s.pids as u64));
                        cx.set_label(note, "");
                    }
                    StatsEvent::Ended { error } => {
                        self.stats_sub = None;
                        cx.set_label(note, error.unwrap_or_else(|| t.stats_ended().to_owned()));
                    }
                }
            }
            UiEvent::ExecStarted { container, result } => {
                // A start nobody waits for any more (the container is gone) is released.
                let Some(shell) = self.exec.shells.get_mut(&container).filter(|s| s.starting)
                else {
                    if let Ok(sub) = result {
                        commands::unsubscribe(cx.backend, sub);
                    }
                    return;
                };
                shell.starting = false;
                match result {
                    Ok(sub) => {
                        shell.sub = Some(sub);
                        let term = shell.terminal;
                        // The size it was laid out at, now that the shell can take it.
                        if let Some((cols, rows)) =
                            cx.tree.get::<Terminal>(term).map(|t| t.grid_size())
                        {
                            let _ =
                                commands::exec_resize(cx.backend, sub, cols as u16, rows as u16);
                        }
                        if self.tab == Tab::Exec && self.container.as_deref() == Some(&container) {
                            cx.tree.set_focus(Some(term));
                        }
                    }
                    Err(e) => shell.ended = Some(format!("{}: {}", t.exec_failed(), e.message)),
                }
                self.sync_exec(cx);
            }
            UiEvent::Exec(sub, out) => {
                let Some(shell) = self.exec.by_sub(sub) else {
                    return commands::unsubscribe(cx.backend, sub);
                };
                match out {
                    ExecOutput::Data { base64 } => {
                        commands::ack_stream(cx.backend, sub, 1);
                        let term = shell.terminal;
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(base64)
                        {
                            Self::feed_terminal(term, sub, &bytes, cx);
                        }
                    }
                    ExecOutput::Exit => {
                        shell.sub = None;
                        shell.ended = Some(t.exec_exited().to_owned());
                        self.sync_exec(cx);
                    }
                }
            }
            UiEvent::Inspect { id, result } => {
                if self.tab != Tab::Inspect || self.container.as_deref() != Some(id.as_str()) {
                    return;
                }
                match result {
                    Ok(json) => {
                        self.inspect_json = Some(json);
                        self.show_inspect(cx);
                    }
                    Err(e) => {
                        if let Some(v) = self.inspect_text {
                            cx.tree
                                .update::<TextView, _>(v, |v, ucx| v.set_text(ucx, e.message));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Keys typed in terminal `term`: to its shell, or (Enter) a new shell once it
    /// ended. Returns whether `term` is a shell's.
    fn terminal_input(&mut self, term: WidgetId, bytes: &[u8], cx: &mut Ctx) -> bool {
        let Some(shell) = self.exec.shells.values().find(|s| s.terminal == term) else {
            return false;
        };
        match shell.sub {
            Some(sub) => {
                let r = commands::exec_input(cx.backend, sub, bytes.to_vec());
                cx.attempt(r);
            }
            None if shell.ended.is_some() && bytes.contains(&b'\r') => self.start_shell(cx),
            None => {}
        }
        true
    }

    /// Returns whether the message was the pane's.
    fn handle(&mut self, m: &Message, cx: &mut Ctx) -> bool {
        let src = m.source;
        if src == self.tabs {
            if let Some(SelectionChanged(i)) = m.downcast_ref::<SelectionChanged>() {
                self.open_tab(TABS[*i], cx);
            }
            return true;
        }
        if src == self.expand && m.downcast_ref::<ButtonClicked>().is_some() {
            self.set_expanded(!self.expanded, cx);
            return true;
        }
        if let Some(TerminalInput(bytes)) = m.downcast_ref::<TerminalInput>()
            && self.terminal_input(src, bytes, cx)
        {
            return true;
        }
        let submitted = [self.exec.command, self.exec.user, self.exec.working_dir].contains(&src)
            && m.downcast_ref::<TextSubmitted>().is_some();
        if submitted || (src == self.exec.open && m.downcast_ref::<ButtonClicked>().is_some()) {
            self.start_shell(cx);
            return true;
        }
        if Some(src) == self.log_search {
            if let Some(TextChanged(query)) = m.downcast_ref::<TextChanged>() {
                let action = FindAction::Search {
                    query: query.clone(),
                    options: FindOptions::default(),
                };
                self.find_in_logs(action, cx);
            } else if m.downcast_ref::<SearchSubmitted>().is_some() {
                self.find_in_logs(FindAction::Next, cx);
            }
            return true;
        }
        if Some(src) == self.secrets {
            self.show_inspect(cx);
            return true;
        }
        if Some(src) == self.log_view {
            return true;
        }
        if m.downcast_ref::<ButtonClicked>().is_none() {
            return false;
        }
        if Some(src) == self.clear {
            self.logs.clear();
            self.logs_shown = self.logs.finished();
            if let Some(view) = self.log_view {
                cx.tree.update::<LogView, _>(view, |v, ucx| v.clear(ucx));
            }
            self.refresh_logs(cx);
            return true;
        }
        if Some(src) == self.copy_logs
            && let Some(view) = self.log_view
        {
            let text = cx
                .tree
                .get::<LogView>(view)
                .map(|v| v.copy_text().unwrap_or_else(|| v.all_text()))
                .unwrap_or_default();
            cx.tree.clipboard().write_text(&text);
            cx.toast(cx.t.copied());
            return true;
        }
        if Some(src) == self.copy_raw || Some(src) == self.copy_redacted {
            if let Some(json) = &self.inspect_json {
                let text = if Some(src) == self.copy_raw {
                    json.clone()
                } else {
                    model::redact_inspect(json)
                };
                cx.tree.clipboard().write_text(&text);
                cx.toast(cx.t.copied());
            }
            return true;
        }
        false
    }
}

// ---- resource lists ----

/// A resource list whose rows end in buttons (see [`List::ACTIONS`]).
struct List {
    empty: Note,
    table: Grid,
}

/// A key pressed over a [`List`] (see [`List::shortcut`]).
enum ListKey {
    /// Esc: the selection was cleared.
    Clear,
    /// Delete with one row selected: ask as its row's remove button does.
    RemoveOne,
    /// Delete with several rows selected: ask as "Remove selected" does.
    RemoveSelected,
}

/// What a message did to a [`List`]'s table.
enum ListEvent {
    Selection,
    /// A row's button: the row is selected first.
    Action(String),
}

impl List {
    /// The title of the buttons column every list ends with.
    fn actions_column(t: crate::ui::i18n::Tr, width: f32) -> TableColumn {
        TableColumn::new(t.actions(), width).align(ColumnAlign::Trailing)
    }

    fn build(root: WidgetId, columns: Vec<TableColumn>, cx: &mut Ctx) -> Self {
        let empty = Note::new(cx.tree, root);
        let table = Grid::new(cx.tree, root, columns);
        Self { empty, table }
    }

    fn set(&mut self, cx: &mut Ctx, rows: Vec<GridRow>, empty: &str) {
        let none = rows.is_empty();
        self.table.set_rows(cx.tree, rows);
        self.empty.set(cx, if none { empty } else { "" });
    }

    fn selected(&self) -> Option<&str> {
        self.table.selected.as_deref()
    }

    /// What Esc and Delete ask of the list, as on the containers page: clear the
    /// selection, or remove it (`None` for other keys, and for Delete while
    /// disconnected or with nothing selected).
    fn shortcut(&mut self, shortcut: Shortcut, cx: &mut Ctx) -> Option<ListKey> {
        if shortcut == Shortcut::key(Key::Escape) {
            self.table.clear_selection(cx.tree);
            return Some(ListKey::Clear);
        }
        if shortcut != Shortcut::key(Key::Delete) || !cx.connected() {
            return None;
        }
        match self.table.selected_keys(cx.tree).len() {
            0 => None,
            1 => Some(ListKey::RemoveOne),
            _ => Some(ListKey::RemoveSelected),
        }
    }

    /// Handles the table's messages; `None` when the message is not the table's.
    fn handle_table(&mut self, m: &Message, cx: &mut Ctx) -> Option<ListEvent> {
        if m.source != self.table.id {
            return None;
        }
        if let Some(click) = m.downcast_ref::<CellActionClicked>() {
            let row = click.row;
            cx.tree
                .update::<TableView, _>(self.table.id, |t, ucx| t.select_rows(ucx, [row]));
            self.table.sync_selection(cx.tree);
            return Some(ListEvent::Action(click.action.to_string()));
        }
        self.table.sync_selection(cx.tree);
        Some(ListEvent::Selection)
    }
}

/// "Select all" and "Remove selected (n)" in a page's header, acting on a [`Grid`].
struct BulkBar {
    select_all: WidgetId,
    remove: WidgetId,
}

impl BulkBar {
    fn build(cx: &mut Ctx, head: WidgetId) -> Self {
        let t = cx.t;
        let select_all = button(cx.tree, head, t.select_all());
        let remove = button(cx.tree, head, t.remove_selected(0));
        Self { select_all, remove }
    }

    /// Follows the selection: "Select all" turns into "Deselect all" once every row is
    /// selected, and "Remove selected" counts the `removable` selected rows.
    fn sync(&self, cx: &mut Ctx, grid: &Grid, removable: usize) {
        let t = cx.t;
        let total = grid.selectable_count();
        let all = total > 0 && grid.selected_keys(cx.tree).len() == total;
        let select_label = if all {
            t.deselect_all()
        } else {
            t.select_all()
        };
        for (id, label) in [
            (self.select_all, select_label.to_owned()),
            (self.remove, t.remove_selected(removable)),
        ] {
            cx.tree.update::<Button, _>(id, |b, ucx| {
                if b.label() != label {
                    b.set_label(ucx, label);
                }
            });
        }
        cx.enable(self.select_all, total > 0);
        let connected = cx.connected();
        cx.enable(self.remove, connected && removable > 0);
    }

    /// Toggles between all rows selected and none when `m` is a click on "Select all".
    fn toggle(&self, m: &Message, grid: &mut Grid, tree: &mut UiTree) -> bool {
        if m.source != self.select_all || m.downcast_ref::<ButtonClicked>().is_none() {
            return false;
        }
        if grid.selected_keys(tree).len() == grid.selectable_count() {
            grid.clear_selection(tree);
        } else {
            grid.select_all(tree);
        }
        true
    }

    /// Whether `m` is a click on "Remove selected".
    fn is_remove(&self, m: &Message) -> bool {
        m.source == self.remove && m.downcast_ref::<ButtonClicked>().is_some()
    }
}

fn usage_text(cx: &Ctx, users: usize) -> String {
    if users > 0 {
        cx.t.in_use_by(users)
    } else {
        cx.t.unused().to_owned()
    }
}

/// Inspect JSON beside a list (images, volumes, networks), read-only.
struct InspectPane {
    root: WidgetId,
    title: WidgetId,
    close: WidgetId,
    text: WidgetId,
    /// What the pane shows; a reply for anything else is dropped.
    inspecting: Option<String>,
}

impl InspectPane {
    fn build(body: WidgetId, cx: &mut Ctx) -> Self {
        let t = cx.t;
        let (root, inner) = side_pane(cx.tree, body);
        let bar = cx.tree.insert(Some(inner), widgets::row(tokens::SPACE_3));
        let title = cx.tree.insert(
            Some(bar),
            Label::new("")
                .weight(tokens::WEIGHT_STRONG)
                .font_family(widgets::MONO),
        );
        cx.tree.insert(Some(bar), widgets::spacer());
        let close = button(cx.tree, bar, t.close());
        let text = cx.tree.insert(
            Some(inner),
            TextView::new(0.0).fill_height().read_only(true),
        );
        cx.tree.set_accessible_label(text, t.inspect_output());
        Self {
            root,
            title,
            close,
            text,
            inspecting: None,
        }
    }

    /// Shows the pane for `label` and asks the engine for `id`'s JSON.
    fn open(&mut self, kind: InspectKind, id: String, label: &str, cx: &mut Ctx) {
        let t = cx.t;
        self.inspecting = Some(id.clone());
        cx.show(self.root, true);
        cx.set_label(self.title, t.inspect_title(label));
        cx.tree
            .update::<TextView, _>(self.text, |v, ucx| v.set_text(ucx, t.loading()));
        let backend = cx.backend.clone();
        cx.backend.spawn(async move {
            let result = commands::inspect(backend.clone(), kind, id.clone()).await;
            backend.send(UiEvent::Inspect { id, result });
        });
    }

    fn hide(&mut self, cx: &mut Ctx) {
        self.inspecting = None;
        cx.show(self.root, false);
    }

    /// Handles Close; false when `m` is not the pane's.
    fn handle(&mut self, m: &Message, cx: &mut Ctx) -> bool {
        if m.source == self.close && m.downcast_ref::<ButtonClicked>().is_some() {
            self.hide(cx);
            return true;
        }
        false
    }

    /// Shows the engine's reply for `id`, if the pane still shows it.
    fn show(&self, id: &str, result: Result<String, UiError>, cx: &mut Ctx) {
        if self.inspecting.as_deref() == Some(id) {
            let shown = result.unwrap_or_else(|e| e.message);
            cx.tree
                .update::<TextView, _>(self.text, |v, ucx| v.set_text(ucx, shown));
        }
    }
}

/// `docker run -d` beside the images list, with the options people set most: name,
/// command, published ports, named volumes, network, restart policy and environment.
/// `run_spec` checks the fields; host paths cannot be mounted.
struct RunPane {
    root: WidgetId,
    title: WidgetId,
    run: WidgetId,
    close: WidgetId,
    name: WidgetId,
    command: WidgetId,
    ports: WidgetId,
    volumes: WidgetId,
    network: WidgetId,
    restart: WidgetId,
    env: WidgetId,
    /// The image reference the form runs.
    image: Option<String>,
    restart_choice: RestartChoice,
}

impl RunPane {
    fn build(body: WidgetId, cx: &mut Ctx) -> Self {
        let t = cx.t;
        let (root, inner) = side_pane(cx.tree, body);
        let bar = cx.tree.insert(Some(inner), widgets::row(tokens::SPACE_3));
        let title = cx.tree.insert(
            Some(bar),
            Label::new("")
                .weight(tokens::WEIGHT_STRONG)
                .font_family(widgets::MONO),
        );
        cx.tree.insert(Some(bar), widgets::spacer());
        let run = button(cx.tree, bar, t.run());
        let close = button(cx.tree, bar, t.close());
        // The hint says what to type; the name (for screen readers) stays once it goes.
        let field = |cx: &mut Ctx, hint: &'static str, name: &'static str| {
            let id = cx
                .tree
                .insert(Some(inner), TextField::new(hint, tokens::FIELD_WIDE));
            cx.tree.set_accessible_label(id, name);
            id
        };
        let name = field(cx, t.run_name(), t.run_name_label());
        let command = field(cx, t.run_command(), t.run_command_label());
        let ports = field(cx, t.run_ports(), t.run_ports_label());
        let volumes = field(cx, t.run_volumes(), t.run_volumes_label());
        let network = field(cx, t.run_network(), t.run_network_label());
        let row = cx.tree.insert(Some(inner), widgets::row(tokens::SPACE_3));
        cx.tree.insert(
            Some(row),
            Label::new(t.run_restart()).font_size(tokens::TEXT_BASE),
        );
        let restart = cx.tree.insert(
            Some(row),
            SegmentedControl::new(
                RestartChoice::ALL.map(RestartChoice::label),
                SegmentMode::SelectOne,
            )
            .with_selected(&[0]),
        );
        let env = cx.tree.insert(
            Some(inner),
            TextView::new(0.0).fill_height().placeholder(t.run_env()),
        );
        cx.tree.set_accessible_label(env, t.run_env_label());
        muted(cx.tree, inner, t.run_hint());
        Self {
            root,
            title,
            run,
            close,
            name,
            command,
            ports,
            volumes,
            network,
            restart,
            env,
            image: None,
            restart_choice: RestartChoice::No,
        }
    }

    fn fields(&self) -> [WidgetId; 5] {
        [
            self.name,
            self.command,
            self.ports,
            self.volumes,
            self.network,
        ]
    }

    /// Opens the form for `image`. The fields are kept while it stays the same image,
    /// so a run the daemon refused can be fixed and tried again.
    fn open(&mut self, image: &str, cx: &mut Ctx) {
        let t = cx.t;
        if self.image.as_deref() != Some(image) {
            for id in self.fields() {
                cx.tree
                    .update::<TextField, _>(id, |f, ucx| f.set_text(ucx, ""));
            }
            cx.tree
                .update::<TextView, _>(self.env, |v, ucx| v.set_text(ucx, ""));
            cx.tree
                .update::<SegmentedControl, _>(self.restart, |s, ucx| s.set_selected(ucx, &[0]));
            self.restart_choice = RestartChoice::No;
            self.image = Some(image.to_owned());
        }
        cx.set_label(self.title, t.run_title(image));
        cx.show(self.root, true);
        self.sync(cx);
        cx.tree.set_focus(Some(self.name));
    }

    fn hide(&mut self, cx: &mut Ctx) {
        cx.show(self.root, false);
    }

    /// Opens the form filled in from `template` (a Duplicate), to change and run.
    fn fill(&mut self, template: &RunTemplate, cx: &mut Ctx) {
        let t = cx.t;
        let form = &template.form;
        for (id, text) in [
            (self.name, &form.name),
            (self.command, &form.command),
            (self.ports, &form.ports),
            (self.volumes, &form.volumes),
            (self.network, &form.network),
        ] {
            let text = text.clone();
            cx.tree
                .update::<TextField, _>(id, move |f, ucx| f.set_text(ucx, text.as_str()));
        }
        let env = form.env.clone();
        cx.tree
            .update::<TextView, _>(self.env, |v, ucx| v.set_text(ucx, env));
        let index = RestartChoice::ALL
            .iter()
            .position(|c| *c == form.restart)
            .unwrap_or(0);
        cx.tree
            .update::<SegmentedControl, _>(self.restart, |s, ucx| s.set_selected(ucx, &[index]));
        self.restart_choice = form.restart;
        self.image = Some(form.image.clone());
        cx.set_label(
            self.title,
            format!("{} · {}", t.duplicate_title(&template.source), form.image),
        );
        cx.show(self.root, true);
        self.sync(cx);
        cx.tree.set_focus(Some(self.name));
    }

    fn sync(&self, cx: &mut Ctx) {
        cx.enable(self.run, cx.connected());
    }

    fn form(&self, tree: &UiTree) -> RunForm {
        let field = |id: WidgetId| {
            tree.get::<TextField>(id)
                .map(|f| f.text().to_owned())
                .unwrap_or_default()
        };
        RunForm {
            image: self.image.clone().unwrap_or_default(),
            name: field(self.name),
            command: field(self.command),
            ports: field(self.ports),
            env: tree
                .get::<TextView>(self.env)
                .map(|v| v.text().to_owned())
                .unwrap_or_default(),
            volumes: field(self.volumes),
            network: field(self.network),
            restart: self.restart_choice,
        }
    }

    /// Handles the form's controls; false when `m` is not the pane's.
    fn handle(&mut self, m: &Message, cx: &mut Ctx) -> bool {
        let src = m.source;
        if src == self.restart
            && let Some(SelectionChanged(i)) = m.downcast_ref::<SelectionChanged>()
        {
            if let Some(&choice) = RestartChoice::ALL.get(*i) {
                self.restart_choice = choice;
            }
            return true;
        }
        let clicked = m.downcast_ref::<ButtonClicked>().is_some();
        if src == self.close && clicked {
            self.hide(cx);
            return true;
        }
        let submit = (src == self.run && clicked)
            || (self.fields().contains(&src) && m.downcast_ref::<TextSubmitted>().is_some());
        if !submit {
            return false;
        }
        let form = self.form(cx.tree);
        match commands::run_container(cx.backend, &form) {
            Ok(()) => self.hide(cx),
            Err(e) => cx.toast(e.message),
        }
        true
    }
}

pub struct ImagesView {
    reference: WidgetId,
    pull: WidgetId,
    total: WidgetId,
    prune: WidgetId,
    bulk: BulkBar,
    pulls: Note,
    list: List,
    inspect: InspectPane,
    run_pane: RunPane,
}

impl ImagesView {
    fn build(root: WidgetId, cx: &mut Ctx) -> Self {
        let t = cx.t;
        let head = header(cx, root, t.nav_images());
        cx.tree.insert(Some(head), widgets::spacer());
        let total = muted(cx.tree, head, "");
        let bulk = BulkBar::build(cx, head);
        let reference = cx.tree.insert(
            Some(head),
            TextField::new(t.pull_placeholder(), tokens::FIELD_WIDE),
        );
        cx.tree.set_accessible_label(reference, t.pull_name());
        let pull = button(cx.tree, head, t.pull());
        let prune = button(cx.tree, head, t.prune_dangling());
        let pulls = Note::new(cx.tree, root);
        let (body, left) = split(cx.tree, root);
        let list = List::build(
            left,
            vec![
                TableColumn::new(t.col_repo_tag(), 240.0)
                    .sort(ColumnSort::Text)
                    .flex(1.8)
                    .shrink_to(120.0),
                TableColumn::new(t.col_short_id(), 120.0)
                    .font_family(widgets::MONO)
                    .sort(ColumnSort::Text),
                TableColumn::new(t.col_size(), 90.0)
                    .font_family(widgets::MONO)
                    .align(ColumnAlign::Trailing)
                    .sort(ColumnSort::Number),
                TableColumn::new(t.col_created(), 120.0).sort(ColumnSort::Number),
                TableColumn::new(t.col_usage(), 110.0)
                    .font_family(widgets::MONO)
                    .sort(ColumnSort::Text),
                List::actions_column(t, actions_width(3)),
            ],
            cx,
        );
        let inspect = InspectPane::build(body, cx);
        let run_pane = RunPane::build(body, cx);
        Self {
            reference,
            pull,
            total,
            prune,
            bulk,
            pulls,
            list,
            inspect,
            run_pane,
        }
    }

    fn refresh(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        let connected = cx.connected();
        let now = now_secs();
        let mut images = cx.snap.images.clone();
        images.sort_by(|a, b| b.created.total_cmp(&a.created));
        let rows = images
            .iter()
            .map(|i| {
                let tag = if i.tags.is_empty() {
                    "<none>:<none>".to_owned()
                } else {
                    i.tags.join(", ")
                };
                let users = i.containers as usize;
                GridRow::new(
                    i.id.clone(),
                    vec![
                        tag,
                        i.short_id.clone(),
                        model::format_bytes(i.size),
                        model::relative_time(i.created, now, t),
                        usage_text(cx, users),
                        String::new(),
                    ],
                )
                .sort_number(2, Some(i.size))
                .sort_number(3, Some(i.created))
                .look(4, usage_look(users))
                .buttons(
                    5,
                    vec![
                        row_button(t, "run", &icons().play).enabled(connected),
                        row_button(t, "inspect", &icons().inspect).enabled(connected),
                        row_button(t, "remove", &icons().trash).enabled(connected),
                    ],
                )
            })
            .collect();
        self.list.set(cx, rows, t.no_images());
        let total: f64 = cx.snap.images.iter().map(|i| i.size).sum();
        cx.set_label(self.total, t.total_size(&model::format_bytes(total)));
        let pulls = cx
            .snap
            .pulls
            .iter()
            .map(|p| {
                let pct = p
                    .fraction
                    .map_or(String::new(), |f| format!(" {:.0}%", f * 100.0));
                format!("{}{pct} — {}", t.pulling(&p.reference), p.status)
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.pulls.set(cx, &pulls);
        cx.enable(self.pull, connected);
        cx.enable(self.prune, connected);
        self.run_pane.sync(cx);
        self.sync_bulk_buttons(cx);
    }

    fn sync_bulk_buttons(&mut self, cx: &mut Ctx) {
        let n = self.list.table.selected_keys(cx.tree).len();
        self.bulk.sync(cx, &self.list.table, n);
    }

    fn handle(&mut self, m: &Message, cx: &mut Ctx) {
        let t = cx.t;
        if self.run_pane.handle(m, cx) || self.inspect.handle(m, cx) {
            return;
        }
        match self.list.handle_table(m, cx) {
            Some(ListEvent::Action(action)) => {
                self.sync_bulk_buttons(cx);
                return self.act(&action, cx);
            }
            Some(ListEvent::Selection) => return self.sync_bulk_buttons(cx),
            None => {}
        }
        if self.bulk.toggle(m, &mut self.list.table, cx.tree) {
            return self.sync_bulk_buttons(cx);
        }
        if self.bulk.is_remove(m) {
            return self.ask_remove_selected(cx);
        }
        let src = m.source;
        let submit = (src == self.reference && m.downcast_ref::<TextSubmitted>().is_some())
            || (src == self.pull && m.downcast_ref::<ButtonClicked>().is_some());
        if submit {
            let reference = cx
                .tree
                .get::<TextField>(self.reference)
                .map(|f| f.text().to_owned())
                .unwrap_or_default();
            match commands::pull(cx.backend, &reference) {
                Ok(()) => {
                    cx.tree
                        .update::<TextField, _>(self.reference, |f, ucx| f.set_text(ucx, ""));
                }
                Err(e) => cx.toast(e.message),
            }
            return;
        }
        if m.downcast_ref::<ButtonClicked>().is_none() {
            return;
        }
        if src == self.prune {
            let dangling: Vec<_> = cx.snap.images.iter().filter(|i| i.dangling).collect();
            if dangling.is_empty() {
                return cx.toast(t.no_dangling());
            }
            let size = model::format_bytes(dangling.iter().map(|i| i.size).sum());
            let body = t.prune_dangling_body(dangling.len(), &size);
            cx.confirm(
                t.prune_dangling_title(),
                &body,
                t.prune(),
                Act::Prune(PruneKind::Images),
            );
        }
    }

    fn shortcut(&mut self, shortcut: Shortcut, cx: &mut Ctx) {
        match self.list.shortcut(shortcut, cx) {
            Some(ListKey::Clear) => {
                self.inspect.hide(cx);
                self.sync_bulk_buttons(cx);
            }
            Some(ListKey::RemoveOne) => self.act("remove", cx),
            Some(ListKey::RemoveSelected) => self.ask_remove_selected(cx),
            None => {}
        }
    }

    /// A row's button, on the selected image.
    fn act(&mut self, action: &str, cx: &mut Ctx) {
        let t = cx.t;
        let Some(img) = self
            .list
            .selected()
            .and_then(|id| cx.snap.images.iter().find(|i| i.id == id))
            .cloned()
        else {
            return;
        };
        let label = img
            .tags
            .first()
            .cloned()
            .unwrap_or_else(|| img.short_id.clone());
        if action == "run" {
            // A dangling image has no tag to run by; its id works as well.
            let reference = img.tags.first().cloned().unwrap_or_else(|| img.id.clone());
            self.inspect.hide(cx);
            self.run_pane.open(&reference, cx);
        } else if action == "inspect" {
            self.run_pane.hide(cx);
            self.inspect
                .open(InspectKind::Image, img.id.clone(), &label, cx);
        } else if action == "remove" {
            let in_use = img.containers > 0.0;
            let body = if in_use {
                t.remove_image_in_use(&label)
            } else {
                t.remove_image_body(&label)
            };
            cx.confirm(
                t.remove_image_title(),
                &body,
                t.remove(),
                Act::RemoveImage(img.id, in_use),
            );
        }
    }

    /// Asks once for every selected image; images used by a container are forced, as
    /// a single removal of one is.
    fn ask_remove_selected(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        let keys = self.list.table.selected_keys(cx.tree);
        let images: Vec<(String, bool)> = cx
            .snap
            .images
            .iter()
            .filter(|i| keys.contains(&i.id))
            .map(|i| (i.id.clone(), i.containers > 0.0))
            .collect();
        if images.is_empty() {
            return;
        }
        let in_use = images.iter().filter(|(_, used)| *used).count();
        let body = if in_use > 0 {
            format!(
                "{} {}",
                t.remove_images_body(images.len()),
                t.remove_images_in_use(in_use)
            )
        } else {
            t.remove_images_body(images.len())
        };
        cx.confirm(
            &t.remove_images_title(images.len()),
            &body,
            t.remove(),
            Act::RemoveImages(images),
        );
    }
}

pub struct VolumesView {
    name: WidgetId,
    create: WidgetId,
    prune: WidgetId,
    bulk: BulkBar,
    list: List,
    inspect: InspectPane,
}

impl VolumesView {
    fn build(root: WidgetId, cx: &mut Ctx) -> Self {
        let t = cx.t;
        let head = header(cx, root, t.nav_volumes());
        cx.tree.insert(Some(head), widgets::spacer());
        let bulk = BulkBar::build(cx, head);
        let name = cx.tree.insert(
            Some(head),
            TextField::new(t.new_volume(), tokens::FIELD_NAME),
        );
        cx.tree.set_accessible_label(name, t.new_volume());
        let create = button(cx.tree, head, t.create());
        let prune = button(cx.tree, head, t.prune_unused());
        let (body, left) = split(cx.tree, root);
        let list = List::build(
            left,
            vec![
                TableColumn::new(t.col_volume(), 200.0)
                    .sort(ColumnSort::Text)
                    .flex(2.0)
                    .shrink_to(120.0),
                TableColumn::new(t.col_stack(), 120.0).sort(ColumnSort::Text),
                TableColumn::new(t.col_driver(), 80.0).sort(ColumnSort::Text),
                TableColumn::new(t.col_size(), 90.0)
                    .font_family(widgets::MONO)
                    .align(ColumnAlign::Trailing)
                    .sort(ColumnSort::Number),
                TableColumn::new(t.col_usage(), 110.0)
                    .font_family(widgets::MONO)
                    .sort(ColumnSort::Text),
                TableColumn::new(t.col_mountpoint(), 260.0)
                    .sort(ColumnSort::Text)
                    .font_family(widgets::MONO)
                    .flex(1.0),
                List::actions_column(t, actions_width(2)),
            ],
            cx,
        );
        let inspect = InspectPane::build(body, cx);
        Self {
            name,
            create,
            prune,
            bulk,
            list,
            inspect,
        }
    }

    fn sync_bulk_buttons(&mut self, cx: &mut Ctx) {
        let n = self.list.table.selected_keys(cx.tree).len();
        self.bulk.sync(cx, &self.list.table, n);
    }

    fn api_version(cx: &Ctx) -> Option<String> {
        match &cx.snap.conn {
            ConnState::Connected { info } => Some(info.api_version.clone()),
            _ => None,
        }
    }

    fn refresh(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        let connected = cx.connected();
        let used = model::usage_counts(&cx.snap.containers, |c| &c.volumes);
        let mut volumes = cx.snap.volumes.clone();
        volumes.sort_by(|a, b| a.name.cmp(&b.name));
        let rows = volumes
            .iter()
            .map(|v| {
                let stack = match (&v.compose_project, v.anonymous) {
                    (Some(p), _) => p.clone(),
                    (None, true) => t.anonymous().to_owned(),
                    (None, false) => String::new(),
                };
                let users = used.get(v.name.as_str()).copied().unwrap_or(0);
                GridRow::new(
                    v.name.clone(),
                    vec![
                        v.name.clone(),
                        stack,
                        v.driver.clone(),
                        v.size.map_or("—".into(), model::format_bytes),
                        usage_text(cx, users),
                        v.mountpoint.clone(),
                        String::new(),
                    ],
                )
                .sort_number(3, v.size)
                .look(4, usage_look(users))
                .buttons(
                    6,
                    vec![
                        row_button(t, "inspect", &icons().inspect).enabled(connected),
                        row_button(t, "remove", &icons().trash).enabled(connected),
                    ],
                )
            })
            .collect();
        self.list.set(cx, rows, t.no_volumes());
        cx.enable(self.create, connected);
        cx.enable(self.prune, connected);
        self.sync_bulk_buttons(cx);
    }

    fn handle(&mut self, m: &Message, cx: &mut Ctx) {
        let t = cx.t;
        match self.list.handle_table(m, cx) {
            Some(ListEvent::Action(action)) if action == "remove" => {
                self.sync_bulk_buttons(cx);
                return self.ask_remove(cx);
            }
            Some(ListEvent::Action(action)) if action == "inspect" => {
                self.sync_bulk_buttons(cx);
                if let Some(name) = self.list.selected().map(str::to_owned) {
                    self.inspect
                        .open(InspectKind::Volume, name.clone(), &name, cx);
                }
                return;
            }
            Some(_) => return self.sync_bulk_buttons(cx),
            None => {}
        }
        if self.inspect.handle(m, cx) {
            return;
        }
        let src = m.source;
        if (src == self.name && m.downcast_ref::<TextSubmitted>().is_some())
            || (src == self.create && m.downcast_ref::<ButtonClicked>().is_some())
        {
            let name = cx
                .tree
                .get::<TextField>(self.name)
                .map(|f| f.text().to_owned())
                .unwrap_or_default();
            match commands::create_volume(cx.backend, &name) {
                Ok(()) => {
                    cx.tree
                        .update::<TextField, _>(self.name, |f, ucx| f.set_text(ucx, ""));
                }
                Err(e) => cx.toast(e.message),
            }
            return;
        }
        if self.bulk.toggle(m, &mut self.list.table, cx.tree) {
            return self.sync_bulk_buttons(cx);
        }
        if self.bulk.is_remove(m) {
            return self.ask_remove_selected(cx);
        }
        if m.downcast_ref::<ButtonClicked>().is_none() {
            return;
        }
        if src == self.prune {
            let used = model::usage_counts(&cx.snap.containers, |c| &c.volumes);
            let all = model::prune_removes_named_volumes(Self::api_version(cx).as_deref());
            let targets: Vec<_> = cx
                .snap
                .volumes
                .iter()
                .filter(|v| !used.contains_key(v.name.as_str()) && (all || v.anonymous))
                .collect();
            if targets.is_empty() {
                return cx.toast(t.no_unused_volumes());
            }
            let size = model::format_bytes(targets.iter().filter_map(|v| v.size).sum());
            let body = t.prune_volumes_body(targets.len(), &size, !all);
            cx.confirm(
                t.prune_volumes_title(),
                &body,
                t.prune(),
                Act::Prune(PruneKind::Volumes),
            );
        }
    }

    fn shortcut(&mut self, shortcut: Shortcut, cx: &mut Ctx) {
        match self.list.shortcut(shortcut, cx) {
            Some(ListKey::Clear) => {
                self.inspect.hide(cx);
                self.sync_bulk_buttons(cx);
            }
            Some(ListKey::RemoveOne) => self.ask_remove(cx),
            Some(ListKey::RemoveSelected) => self.ask_remove_selected(cx),
            None => {}
        }
    }

    fn ask_remove(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        if let Some(name) = self.list.selected().map(str::to_owned) {
            let used = model::usage_counts(&cx.snap.containers, |c| &c.volumes);
            let users = used.get(name.as_str()).copied().unwrap_or(0);
            let body = if users > 0 {
                format!(
                    "{} {}",
                    t.remove_volume_body(),
                    t.remove_volume_in_use(users)
                )
            } else {
                t.remove_volume_body().to_owned()
            };
            cx.confirm(
                &t.remove_volume_title(&name),
                &body,
                t.remove(),
                Act::RemoveVolume(name),
            );
        }
    }

    fn ask_remove_selected(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        let names = self.list.table.selected_keys(cx.tree);
        if names.is_empty() {
            return;
        }
        let used = model::usage_counts(&cx.snap.containers, |c| &c.volumes);
        let in_use = names
            .iter()
            .filter(|n| used.contains_key(n.as_str()))
            .count();
        let body = if in_use > 0 {
            format!(
                "{} {}",
                t.remove_volume_body(),
                t.remove_volumes_in_use(in_use)
            )
        } else {
            t.remove_volume_body().to_owned()
        };
        cx.confirm(
            &t.remove_volumes_title(names.len()),
            &body,
            t.remove(),
            Act::RemoveVolumes(names),
        );
    }
}

pub struct NetworksView {
    name: WidgetId,
    create: WidgetId,
    prune: WidgetId,
    bulk: BulkBar,
    list: List,
    inspect: InspectPane,
}

impl NetworksView {
    fn build(root: WidgetId, cx: &mut Ctx) -> Self {
        let t = cx.t;
        let head = header(cx, root, t.nav_networks());
        cx.tree.insert(Some(head), widgets::spacer());
        let bulk = BulkBar::build(cx, head);
        let name = cx.tree.insert(
            Some(head),
            TextField::new(t.new_network(), tokens::FIELD_NAME),
        );
        cx.tree.set_accessible_label(name, t.new_network());
        let create = button(cx.tree, head, t.create());
        let prune = button(cx.tree, head, t.prune_unused());
        let (body, left) = split(cx.tree, root);
        let list = List::build(
            left,
            vec![
                TableColumn::new(t.col_network(), 200.0)
                    .sort(ColumnSort::Text)
                    .flex(1.4)
                    .shrink_to(120.0),
                TableColumn::new(t.col_driver(), 100.0).sort(ColumnSort::Text),
                TableColumn::new(t.col_scope(), 90.0).sort(ColumnSort::Text),
                TableColumn::new(t.col_flags(), 140.0).sort(ColumnSort::Text),
                TableColumn::new(t.col_usage(), 110.0)
                    .font_family(widgets::MONO)
                    .sort(ColumnSort::Text),
                TableColumn::new("ID", 130.0)
                    .font_family(widgets::MONO)
                    .sort(ColumnSort::Text),
                List::actions_column(t, actions_width(2)),
            ],
            cx,
        );
        let inspect = InspectPane::build(body, cx);
        Self {
            name,
            create,
            prune,
            bulk,
            list,
            inspect,
        }
    }

    /// The selected networks that can be removed (built-in ones cannot).
    fn removable_selection(&self, cx: &Ctx) -> Vec<crate::dto::Network> {
        let keys = self.list.table.selected_keys(cx.tree);
        cx.snap
            .networks
            .iter()
            .filter(|n| !n.builtin && keys.contains(&n.id))
            .cloned()
            .collect()
    }

    fn sync_bulk_buttons(&mut self, cx: &mut Ctx) {
        let n = self.removable_selection(cx).len();
        self.bulk.sync(cx, &self.list.table, n);
    }

    fn refresh(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        let connected = cx.connected();
        let used = model::usage_counts(&cx.snap.containers, |c| &c.networks);
        let mut networks = cx.snap.networks.clone();
        networks.sort_by(|a, b| a.name.cmp(&b.name));
        let rows = networks
            .iter()
            .map(|n| {
                let mut flags = Vec::new();
                if n.builtin {
                    flags.push(t.builtin());
                }
                if n.internal {
                    flags.push(t.internal());
                }
                let users = used.get(n.name.as_str()).copied().unwrap_or(0);
                GridRow::new(
                    n.id.clone(),
                    vec![
                        n.name.clone(),
                        n.driver.clone(),
                        n.scope.clone(),
                        flags.join(", "),
                        usage_text(cx, users),
                        model::short_id(&n.id).to_owned(),
                        String::new(),
                    ],
                )
                .look(4, usage_look(users))
                .buttons(
                    6,
                    vec![
                        row_button(t, "inspect", &icons().inspect).enabled(connected),
                        row_button(t, "remove", &icons().trash).enabled(connected && !n.builtin),
                    ],
                )
            })
            .collect();
        self.list.set(cx, rows, t.no_networks());
        cx.enable(self.create, connected);
        cx.enable(self.prune, connected);
        self.sync_bulk_buttons(cx);
    }

    fn handle(&mut self, m: &Message, cx: &mut Ctx) {
        let t = cx.t;
        match self.list.handle_table(m, cx) {
            Some(ListEvent::Action(action)) if action == "remove" => {
                self.sync_bulk_buttons(cx);
                return self.ask_remove(cx);
            }
            Some(ListEvent::Action(action)) if action == "inspect" => {
                self.sync_bulk_buttons(cx);
                if let Some(net) = self
                    .list
                    .selected()
                    .and_then(|id| cx.snap.networks.iter().find(|n| n.id == id))
                    .cloned()
                {
                    self.inspect
                        .open(InspectKind::Network, net.id, &net.name, cx);
                }
                return;
            }
            Some(_) => return self.sync_bulk_buttons(cx),
            None => {}
        }
        if self.inspect.handle(m, cx) {
            return;
        }
        if self.bulk.toggle(m, &mut self.list.table, cx.tree) {
            return self.sync_bulk_buttons(cx);
        }
        if self.bulk.is_remove(m) {
            return self.ask_remove_selected(cx);
        }
        let src = m.source;
        if (src == self.name && m.downcast_ref::<TextSubmitted>().is_some())
            || (src == self.create && m.downcast_ref::<ButtonClicked>().is_some())
        {
            let name = cx
                .tree
                .get::<TextField>(self.name)
                .map(|f| f.text().to_owned())
                .unwrap_or_default();
            match commands::create_network(cx.backend, &name) {
                Ok(()) => {
                    cx.tree
                        .update::<TextField, _>(self.name, |f, ucx| f.set_text(ucx, ""));
                }
                Err(e) => cx.toast(e.message),
            }
            return;
        }
        if m.downcast_ref::<ButtonClicked>().is_none() {
            return;
        }
        let used = model::usage_counts(&cx.snap.containers, |c| &c.networks);
        if src == self.prune {
            let n = cx
                .snap
                .networks
                .iter()
                .filter(|n| !n.builtin && !used.contains_key(n.name.as_str()))
                .count();
            if n == 0 {
                return cx.toast(t.no_unused_networks());
            }
            cx.confirm(
                t.prune_networks_title(),
                &t.prune_networks_body(n),
                t.prune(),
                Act::Prune(PruneKind::Networks),
            );
        }
    }

    /// Built-in networks are left out, as their remove buttons are disabled.
    fn shortcut(&mut self, shortcut: Shortcut, cx: &mut Ctx) {
        match self.list.shortcut(shortcut, cx) {
            Some(ListKey::Clear) => {
                self.inspect.hide(cx);
                self.sync_bulk_buttons(cx);
            }
            Some(ListKey::RemoveOne) if !self.removable_selection(cx).is_empty() => {
                self.ask_remove(cx)
            }
            Some(ListKey::RemoveSelected) => self.ask_remove_selected(cx),
            _ => {}
        }
    }

    fn ask_remove(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        let used = model::usage_counts(&cx.snap.containers, |c| &c.networks);
        if let Some(net) = self
            .list
            .selected()
            .and_then(|id| cx.snap.networks.iter().find(|n| n.id == id))
            .cloned()
        {
            let users = used.get(net.name.as_str()).copied().unwrap_or(0);
            let body = if users > 0 {
                format!(
                    "{} {}",
                    t.remove_network_body(),
                    t.remove_network_in_use(users)
                )
            } else {
                t.remove_network_body().to_owned()
            };
            cx.confirm(
                &t.remove_network_title(&net.name),
                &body,
                t.remove(),
                Act::RemoveNetwork(net.id),
            );
        }
    }

    fn ask_remove_selected(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        let nets = self.removable_selection(cx);
        if nets.is_empty() {
            return;
        }
        let used = model::usage_counts(&cx.snap.containers, |c| &c.networks);
        let in_use = nets
            .iter()
            .filter(|n| used.contains_key(n.name.as_str()))
            .count();
        let body = if in_use > 0 {
            format!(
                "{} {}",
                t.remove_networks_body(),
                t.remove_networks_in_use(in_use)
            )
        } else {
            t.remove_networks_body().to_owned()
        };
        cx.confirm(
            &t.remove_networks_title(nets.len()),
            &body,
            t.remove(),
            Act::RemoveNetworks(nets.into_iter().map(|n| n.id).collect()),
        );
    }
}

// ---- settings ----

pub struct SettingsView {
    theme: WidgetId,
    language: WidgetId,
    contrast: WidgetId,
    log_lines: WidgetId,
    log_save: WidgetId,
    tray: WidgetId,
    /// The engine facts: target, engine, compose, then the settings file.
    facts: DescriptionListParts,
    source: WidgetId,
    context: WidgetId,
}

impl SettingsView {
    fn build(root: WidgetId, cx: &mut Ctx) -> Self {
        let t = cx.t;
        header(cx, root, t.nav_settings());
        let page = cx
            .tree
            .insert(Some(root), widgets::fill_column(0.0, tokens::PAGE_X));
        let columns = cx.tree.insert(
            Some(page),
            Boxed::new(Style {
                flex_direction: FlexDirection::Row,
                flex_wrap: noa_gpui_platform::noa_gpui_core::style::FlexWrap::Wrap,
                align_items: noa_gpui_platform::noa_gpui_core::style::AlignItems::FLEX_START,
                gap: widgets::spacing(tokens::SPACE_6),
                ..Default::default()
            }),
        );
        let column = |cx: &mut Ctx, width: f32| {
            cx.tree.insert(
                Some(columns),
                Boxed::new(Style {
                    flex_direction: FlexDirection::Column,
                    gap: widgets::spacing(tokens::SPACE_5),
                    size: Size {
                        width: length(width),
                        height: noa_gpui_platform::noa_gpui_core::style::auto(),
                    },
                    ..Default::default()
                }),
            )
        };
        let left = column(cx, tokens::SETTINGS_COLUMN);
        let right = column(cx, tokens::SETTINGS_COLUMN);
        let section = |cx: &mut Ctx, parent: WidgetId, title: &str| {
            let block = cx
                .tree
                .insert(Some(parent), widgets::column(tokens::SPACE_3));
            cx.tree.insert(
                Some(block),
                Label::new(title)
                    .weight(tokens::WEIGHT_STRONG)
                    .font_size(tokens::TEXT_MD),
            );
            block
        };
        let field = |cx: &mut Ctx, parent: WidgetId, name: &str| {
            let row = cx.tree.insert(Some(parent), widgets::row(tokens::SPACE_3));
            cx.tree
                .insert(Some(row), Label::new(name).font_size(tokens::TEXT_BASE));
            row
        };
        // A row whose control starts where its neighbours' do.
        let aligned_field = |cx: &mut Ctx, parent: WidgetId, name: &str| {
            let row = cx.tree.insert(Some(parent), widgets::row(tokens::SPACE_3));
            let label = cx
                .tree
                .insert(Some(row), Boxed::column().width(tokens::SETTING_LABEL));
            cx.tree
                .insert(Some(label), Label::new(name).font_size(tokens::TEXT_BASE));
            row
        };

        let appearance = section(cx, left, t.appearance());
        let row = aligned_field(cx, appearance, t.theme());
        let theme_index = match cx.config.theme {
            ThemePref::System => 0,
            ThemePref::Light => 1,
            ThemePref::Dark => 2,
        };
        let theme = cx.tree.insert(
            Some(row),
            SegmentedControl::new(
                [t.theme_system(), t.theme_light(), t.theme_dark()],
                SegmentMode::SelectOne,
            )
            .with_selected(&[theme_index]),
        );
        let row = aligned_field(cx, appearance, t.language());
        let lang_index = match cx.config.language {
            LanguagePref::System => 0,
            LanguagePref::En => 1,
            LanguagePref::Ja => 2,
        };
        let language = cx.tree.insert(
            Some(row),
            SegmentedControl::new(
                [t.theme_system(), "English", "日本語"],
                SegmentMode::SelectOne,
            )
            .with_selected(&[lang_index]),
        );
        let contrast = cx.tree.insert(
            Some(appearance),
            Checkbox::new(t.increase_contrast()).checked(cx.config.increase_contrast),
        );
        muted(cx.tree, appearance, t.increase_contrast_hint());

        let logs = section(cx, left, t.logs());
        let row = field(cx, logs, t.log_lines());
        let log_lines = cx
            .tree
            .insert(Some(row), TextField::new("10000", tokens::FIELD_NUMBER));
        cx.tree.set_accessible_label(log_lines, t.log_lines());
        let lines = cx.config.log_lines.to_string();
        cx.tree
            .update::<TextField, _>(log_lines, |f, ucx| f.set_text(ucx, lines));
        let log_save = button(cx.tree, row, t.save());
        muted(
            cx.tree,
            logs,
            t.log_lines_range(LOG_LINES_MIN, LOG_LINES_MAX),
        );

        let background = section(cx, left, t.background());
        let tray = cx.tree.insert(
            Some(background),
            Checkbox::new(t.tray_label()).checked(cx.config.tray),
        );
        muted(cx.tree, background, t.tray_hint());

        // Engine facts between rules, under a heavier one.
        let engine = section(cx, left, t.engine());
        cx.tree.insert(
            Some(engine),
            widgets::rule(tokens::RULE_STRONG, Role::Outline),
        );
        let facts = DescriptionList::new()
            .label_width(tokens::FACT_LABEL)
            .label_font_size(tokens::TEXT_SM)
            .label_color(Role::Muted)
            .separators(true)
            .value_font_family(widgets::MONO)
            .value_font_size(tokens::TEXT_SM)
            .items([
                (t.target(), ""),
                (t.engine(), ""),
                (t.compose(), ""),
                (t.settings_file(), t.settings_file_path()),
            ])
            .insert(cx.tree, engine);

        let contexts = section(cx, right, t.context_title());
        let source = cx.tree.insert(
            Some(contexts),
            Label::new("")
                .font_size(tokens::TEXT_SM)
                .color(Role::Muted)
                .font_family(widgets::MONO),
        );
        cx.tree.insert(
            Some(contexts),
            widgets::rule(tokens::RULE_STRONG, Role::Outline),
        );
        let context_row = cx
            .tree
            .insert(Some(contexts), widgets::row(tokens::SPACE_3));
        let context = button(cx.tree, context_row, t.choose_context());
        Self {
            theme,
            language,
            contrast,
            log_lines,
            log_save,
            tray,
            facts,
            source,
            context,
        }
    }

    fn refresh(&mut self, cx: &mut Ctx) {
        let t = cx.t;
        let engine = match &cx.snap.conn {
            ConnState::Connected { info } => format!(
                "{} · API {} · {}/{}",
                t.engine_title(&info.version),
                info.api_version,
                info.os,
                info.arch
            ),
            _ => t.not_connected().to_owned(),
        };
        let target = &cx.snap.target;
        let compose = match cx.snap.compose_available {
            None => t.checking(),
            Some(true) => t.compose_available(),
            Some(false) => t.compose_not_found(),
        };
        let values = [
            format!("{} ({})", target.host, target.source),
            engine,
            compose.to_owned(),
        ];
        for (i, value) in values.into_iter().enumerate() {
            if let Some(&id) = self.facts.values.get(i) {
                cx.set_label(id, value);
            }
        }
        cx.set_label(
            self.source,
            format!("{}: {}", t.context_source(), target.source),
        );
    }

    fn handle(&mut self, m: &Message, cx: &mut Ctx) {
        let src = m.source;
        let mut config = cx.config.clone();
        if src == self.theme
            && let Some(SelectionChanged(i)) = m.downcast_ref::<SelectionChanged>()
        {
            config.theme = [ThemePref::System, ThemePref::Light, ThemePref::Dark][*i];
        } else if src == self.language
            && let Some(SelectionChanged(i)) = m.downcast_ref::<SelectionChanged>()
        {
            config.language = [LanguagePref::System, LanguagePref::En, LanguagePref::Ja][*i];
        } else if src == self.tray
            && let Some(CheckboxChanged(state)) = m.downcast_ref::<CheckboxChanged>()
        {
            config.tray = checked(*state);
        } else if src == self.contrast
            && let Some(CheckboxChanged(state)) = m.downcast_ref::<CheckboxChanged>()
        {
            config.increase_contrast = checked(*state);
        } else if (src == self.log_save && m.downcast_ref::<ButtonClicked>().is_some())
            || (src == self.log_lines && m.downcast_ref::<TextSubmitted>().is_some())
        {
            let text = cx
                .tree
                .get::<TextField>(self.log_lines)
                .map(|f| f.text().trim().to_owned())
                .unwrap_or_default();
            match text.parse::<u32>() {
                Ok(n) => config.log_lines = n.clamp(LOG_LINES_MIN, LOG_LINES_MAX),
                Err(_) => return cx.toast(cx.t.log_lines_range(LOG_LINES_MIN, LOG_LINES_MAX)),
            }
            let shown = config.log_lines.to_string();
            cx.tree
                .update::<TextField, _>(self.log_lines, |f, ucx| f.set_text(ucx, shown));
        } else if src == self.context && m.downcast_ref::<ButtonClicked>().is_some() {
            *cx.open_contexts = true;
            return;
        } else {
            return;
        }
        if config != *cx.config {
            *cx.save_config = Some(config);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use noa_gpui_platform::noa_gpui_core::UiTree;
    use noa_gpui_platform::noa_gpui_scene::Size;
    use noa_gpui_platform::noa_gpui_text::{FontOptions, TextStore};

    use super::*;
    use crate::backend::Backend;
    use crate::config::{Config, ConfigStore};
    use crate::state::AppState;
    use crate::ui::i18n::{Lang, Tr};

    fn subtree(tree: &UiTree, id: WidgetId, out: &mut Vec<WidgetId>) {
        out.push(id);
        for &c in tree.children(id) {
            subtree(tree, c, out);
        }
    }

    /// A header click sorts the rows; the order holds when new data arrives, and the
    /// selection still names the clicked row's key (the table reports data rows).
    #[test]
    fn sorted_grid_keeps_its_order_and_keys() {
        let mut tree = UiTree::new();
        let root = tree.insert(None, widgets::fill_column(0.0, 0.0));
        let mut grid = Grid::new(
            &mut tree,
            root,
            vec![TableColumn::new("Name", 100.0).sort(ColumnSort::Text)],
        );
        let rows = |names: &[&str]| {
            names
                .iter()
                .map(|n| GridRow::new(n.to_string(), vec![n.to_string()]))
                .collect::<Vec<_>>()
        };
        grid.set_rows(&mut tree, rows(&["b", "c", "a"]));
        tree.update::<TableView, _>(grid.id, |t, cx| t.sort_by(cx, 0, false));
        let shown = |tree: &UiTree, grid: &Grid| -> Vec<String> {
            let table = tree.get::<TableView>(grid.id).unwrap();
            table
                .displayed_rows()
                .iter()
                .map(|&r| grid.key_at(r).unwrap().to_owned())
                .collect()
        };
        assert_eq!(shown(&tree, &grid), ["c", "b", "a"]);

        grid.set_rows(&mut tree, rows(&["a", "d", "b"]));
        assert_eq!(shown(&tree, &grid), ["d", "b", "a"]);

        tree.update::<TableView, _>(grid.id, |t, cx| t.select_rows(cx, [1]));
        grid.sync_selection(&tree);
        assert_eq!(grid.selected.as_deref(), Some("d"));
    }

    /// A grid has a checkbox for all its rows in the header and one before each row
    /// that can be selected (not a group row), showing the selection.
    #[test]
    fn grid_rows_have_selection_checkboxes() {
        use noa_gpui_platform::noa_gpui_core::accesskit::{Role, Toggled};

        let mut tree = UiTree::new();
        // A parent with a height, so the table has rows on screen.
        let root = tree.insert(
            None,
            Boxed::new(Style {
                flex_direction: FlexDirection::Column,
                size: noa_gpui_platform::noa_gpui_core::style::Size {
                    width: length(600.0),
                    height: length(400.0),
                },
                ..Default::default()
            }),
        );
        let mut grid = Grid::new(
            &mut tree,
            root,
            vec![TableColumn::new("Name", 100.0).sort(ColumnSort::Text)],
        );
        let row = |name: &str| GridRow::new(name, vec![name.to_owned()]);
        grid.set_rows(
            &mut tree,
            vec![row("stack").group(Vec::new()), row("a"), row("b")],
        );
        tree.update::<TableView, _>(grid.id, |t, cx| t.select_rows(cx, [2]));
        let mut text = TextStore::new(FontOptions::default());
        tree.prepare(Size::new(600.0, 400.0), &mut text);
        let checks: Vec<_> = (tree.accessibility_update(1.0).nodes.iter())
            .filter(|(_, node)| node.role() == Role::CheckBox)
            .map(|(_, node)| (node.label().unwrap().to_owned(), node.toggled().unwrap()))
            .collect();
        let expected = [
            ("Select all rows", Toggled::Mixed),
            ("Select a", Toggled::False),
            ("Select b", Toggled::True),
        ];
        assert_eq!(
            checks,
            expected.map(|(label, state)| (label.to_owned(), state))
        );
    }

    /// Sizes sort by their bytes, not their text ("900 KB" before "1.2 GB"); a row
    /// without a size goes last.
    #[test]
    fn sizes_sort_by_bytes() {
        let mut tree = UiTree::new();
        let root = tree.insert(None, widgets::fill_column(0.0, 0.0));
        let mut grid = Grid::new(
            &mut tree,
            root,
            vec![TableColumn::new("Size", 100.0).sort(ColumnSort::Number)],
        );
        let rows = [("big", Some(1.2e9)), ("none", None), ("small", Some(900e3))]
            .into_iter()
            .map(|(key, bytes)| {
                let text = bytes.map_or("—".into(), model::format_bytes);
                GridRow::new(key, vec![text]).sort_number(0, bytes)
            })
            .collect();
        grid.set_rows(&mut tree, rows);
        tree.update::<TableView, _>(grid.id, |t, cx| t.sort_by(cx, 0, true));
        let table = tree.get::<TableView>(grid.id).unwrap();
        let shown: Vec<_> = table
            .displayed_rows()
            .iter()
            .map(|&r| grid.key_at(r).unwrap())
            .collect();
        assert_eq!(shown, ["small", "big", "none"]);
    }

    /// A page's context with no engine: subscriptions fail, `connected()` follows
    /// `snap.conn`.
    struct Fixture {
        backend: Backend,
        _events: std::sync::mpsc::Receiver<UiEvent>,
        snap: crate::dto::Snapshot,
        config: Config,
        tree: UiTree,
        text: TextStore,
        alerts: HashMap<WidgetId, Vec<Option<Act>>>,
        save: Option<Config>,
        open: bool,
        root: WidgetId,
    }

    impl Fixture {
        fn new() -> Self {
            let target = engine::ConnectTarget {
                host: "unix:///nonexistent.sock".into(),
                source: "test".into(),
                context: None,
                tls: None,
            };
            let (backend, events) =
                Backend::new(AppState::new(target), ConfigStore::new(Config::default()));
            let snap = backend.snapshot();
            let mut tree = UiTree::new();
            let root = tree.insert(None, widgets::fill_column(0.0, 0.0));
            Self {
                backend,
                _events: events,
                snap,
                config: Config::default(),
                tree,
                text: TextStore::new(FontOptions::default()),
                alerts: HashMap::new(),
                save: None,
                open: false,
                root,
            }
        }

        fn cx(&mut self) -> Ctx<'_> {
            Ctx {
                tree: &mut self.tree,
                text: &mut self.text,
                backend: &self.backend,
                snap: &self.snap,
                config: &self.config,
                t: Tr(Lang::En),
                alerts: &mut self.alerts,
                save_config: &mut self.save,
                open_contexts: &mut self.open,
            }
        }

        fn layout(&mut self) {
            self.tree.prepare(Size::new(1200.0, 800.0), &mut self.text);
        }

        fn set_connected(&mut self, connected: bool) {
            self.snap.conn = if connected {
                crate::dto::ConnState::Connected {
                    info: crate::dto::EngineInfo {
                        version: "29".into(),
                        api_version: "1.52".into(),
                        os: "linux".into(),
                        arch: "arm64".into(),
                    },
                }
            } else {
                crate::dto::ConnState::Reconnecting {
                    error: crate::dto::UiError {
                        kind: crate::dto::UiErrorKind::Transport,
                        message: "lost".into(),
                    },
                    retry_in: 1,
                }
            };
        }
    }

    fn running(state: ContainerState) -> Container {
        Container {
            id: "c1".into(),
            name: "web".into(),
            image: "nginx".into(),
            state,
            status: String::new(),
            created: 0.0,
            ports: Vec::new(),
            compose: None,
            networks: Vec::new(),
            volumes: Vec::new(),
        }
    }

    fn log_batch(lines: std::ops::Range<usize>) -> crate::dto::LogBatch {
        crate::dto::LogBatch {
            chunks: vec![crate::dto::LogChunk {
                stream: LogStream::Stdout,
                text: lines.map(|i| format!("line {i}\n")).collect(),
            }],
            ended: None,
        }
    }

    /// Scrolled back through the logs with a line selected, a new batch of lines
    /// leaves the view where it was and the line selected.
    #[test]
    fn new_log_lines_keep_the_scroll_and_the_selection() {
        let mut f = Fixture::new();
        let root = f.root;
        let mut detail = Detail::build(root, root, &mut f.cx());
        detail.container = Some("c1".into());
        f.cx().show(detail.root, true);
        detail.open_tab(Tab::Logs, &mut f.cx());
        detail.log_sub = Some(1);
        detail.stream_event(UiEvent::Logs(1, log_batch(0..300)), &mut f.cx());
        f.layout();
        let view = detail.log_view.unwrap();
        f.tree.update::<LogView, _>(view, |v, cx| {
            v.scroll_to_line(cx, 0);
            v.select_lines(cx, 2..3);
        });
        f.layout();
        // Scrolled back, far from the newest line.
        let before = f.tree.get::<LogView>(view).unwrap().visible_lines();
        assert!(before.end < 100, "{before:?}");

        detail.stream_event(UiEvent::Logs(1, log_batch(300..301)), &mut f.cx());
        f.layout();
        let v = f.tree.get::<LogView>(view).unwrap();
        assert_eq!(v.line_count(), 301);
        assert_eq!(v.selected_lines(), Some(2..3));
        assert_eq!(v.visible_lines(), before);
    }

    /// The engine's streams end when the connection drops; once it is back, the open
    /// tab subscribes again (it is built anew).
    #[test]
    fn the_open_tab_restarts_when_the_connection_comes_back() {
        let mut f = Fixture::new();
        let root = f.root;
        let mut detail = Detail::build(root, root, &mut f.cx());
        let c = running(ContainerState::Running);
        f.set_connected(true);
        detail.set_container(Some(&c), &mut f.cx());
        let first = detail.log_view;

        f.set_connected(false);
        detail.set_container(Some(&c), &mut f.cx());
        f.set_connected(true);
        detail.set_container(Some(&c), &mut f.cx());
        assert_ne!(detail.log_view, first);
    }

    /// The logs of a stopped container end; started again, its logs are followed anew.
    #[test]
    fn logs_follow_a_container_started_again() {
        let mut f = Fixture::new();
        let root = f.root;
        let mut detail = Detail::build(root, root, &mut f.cx());
        f.set_connected(true);
        detail.set_container(Some(&running(ContainerState::Exited)), &mut f.cx());
        let first = detail.log_view;
        // The stream of a stopped container ends after its history.
        detail.log_sub = None;

        detail.set_container(Some(&running(ContainerState::Running)), &mut f.cx());
        assert_ne!(detail.log_view, first);
    }

    /// A detail pane showing running container `c1` on the Exec tab, connected; the
    /// shell it asked for has started as stream `sub`.
    fn exec_detail(f: &mut Fixture) -> (Detail, u32) {
        let root = f.root;
        let mut detail = Detail::build(root, root, &mut f.cx());
        f.set_connected(true);
        detail.tab = Tab::Exec;
        detail.set_container(Some(&running(ContainerState::Running)), &mut f.cx());
        assert!(detail.exec.shells["c1"].starting);
        let sub = f.backend.streams().idle();
        detail.stream_event(
            UiEvent::ExecStarted {
                container: "c1".into(),
                result: Ok(sub),
            },
            &mut f.cx(),
        );
        (detail, sub)
    }

    /// A shell whose start finished while another container was shown keeps running;
    /// one nobody waits for (its container is gone) is released.
    #[test]
    fn a_shell_started_for_another_container_is_kept_and_an_unwanted_one_released() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _enter = runtime.enter();
        let mut f = Fixture::new();
        let (mut detail, _) = exec_detail(&mut f);
        // A's shell was asked for, then c1 was selected again.
        let terminal = f.tree.insert(Some(detail.exec.terminals), Terminal::new());
        detail.exec.shells.insert(
            "a".into(),
            Shell {
                terminal,
                sub: None,
                starting: true,
                ended: None,
            },
        );
        let (a, gone) = (f.backend.streams().idle(), f.backend.streams().idle());
        for (container, sub) in [("a", a), ("gone", gone)] {
            detail.stream_event(
                UiEvent::ExecStarted {
                    container: container.into(),
                    result: Ok(sub),
                },
                &mut f.cx(),
            );
        }
        assert_eq!(detail.exec.shells["a"].sub, Some(a));
        assert!(
            f.tree.is_hidden(terminal),
            "only the shown container's terminal"
        );
        assert_eq!(f.backend.streams().active_count(), 2);
    }

    /// The shell keeps running while another tab is open, and the Exec tab shows it
    /// again rather than a new one.
    #[test]
    fn the_shell_survives_switching_tabs() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _enter = runtime.enter();
        let mut f = Fixture::new();
        let (mut detail, sub) = exec_detail(&mut f);
        let terminal = detail.exec.shells["c1"].terminal;

        detail.open_tab(Tab::Logs, &mut f.cx());
        assert!(f.tree.is_hidden(detail.exec.root));
        assert_eq!(detail.exec.shells["c1"].sub, Some(sub));

        detail.open_tab(Tab::Exec, &mut f.cx());
        assert!(!f.tree.is_hidden(detail.exec.root));
        assert_eq!(detail.exec.shells["c1"].terminal, terminal);
        assert_eq!(detail.exec.shells["c1"].sub, Some(sub));
        assert_eq!(f.backend.streams().active_count(), 1);
    }

    /// Once the shell ended, Enter in its terminal starts a new one with the fields as
    /// typed; other keys do nothing.
    #[test]
    fn enter_after_the_shell_ended_opens_a_new_one() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _enter = runtime.enter();
        let mut f = Fixture::new();
        let (mut detail, sub) = exec_detail(&mut f);
        detail.stream_event(UiEvent::Exec(sub, ExecOutput::Exit), &mut f.cx());
        let ended = detail.exec.shells["c1"].terminal;
        assert!(detail.exec.shells["c1"].ended.is_some());
        assert!(!f.tree.is_hidden(detail.exec.status));
        assert!(f.tree.is_enabled(detail.exec.open));

        assert!(detail.terminal_input(ended, b"x", &mut f.cx()));
        assert_eq!(detail.exec.shells["c1"].terminal, ended);

        assert!(detail.terminal_input(ended, b"\r", &mut f.cx()));
        let shell = &detail.exec.shells["c1"];
        assert!(shell.starting && shell.ended.is_none());
        assert_ne!(shell.terminal, ended);
    }

    /// Expand hides the list so the pane takes the width; closing the pane gives it
    /// back.
    #[test]
    fn expand_gives_the_pane_the_lists_width_until_it_closes() {
        let mut f = Fixture::new();
        let root = f.root;
        let list = f.tree.insert(Some(root), widgets::fill_column(0.0, 0.0));
        let mut detail = Detail::build(root, list, &mut f.cx());
        detail.set_container(Some(&running(ContainerState::Exited)), &mut f.cx());
        for m in click(&mut f, detail.expand) {
            detail.handle(&m, &mut f.cx());
        }
        assert!(detail.expanded);
        assert!(f.tree.is_hidden(list));

        detail.set_container(None, &mut f.cx());
        assert!(!detail.expanded);
        assert!(!f.tree.is_hidden(list));
    }

    /// A shell that finished starting after the page was left is released.
    #[test]
    fn a_shell_started_after_leaving_the_page_is_released() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _enter = runtime.enter();
        let mut f = Fixture::new();
        let root = f.root;
        let mut view = PageView::build(Page::Images, root, BTreeSet::new(), &mut f.cx());
        let sub = f.backend.streams().idle();
        view.stream_event(
            UiEvent::ExecStarted {
                container: "c1".into(),
                result: Ok(sub),
            },
            &mut f.cx(),
        );
        assert_eq!(f.backend.streams().active_count(), 0);
    }

    fn volume(name: &str) -> crate::dto::Volume {
        crate::dto::Volume {
            name: name.into(),
            driver: "local".into(),
            mountpoint: format!("/var/lib/docker/volumes/{name}/_data"),
            created_at: None,
            compose_project: None,
            anonymous: false,
            size: None,
        }
    }

    /// Clicks `id` and returns the messages it produced.
    fn click(f: &mut Fixture, id: WidgetId) -> Vec<Message> {
        use noa_gpui_platform::noa_gpui_core::{Event, PointerButton};
        f.layout();
        let r = f.tree.window_rect(id).unwrap();
        let pos = noa_gpui_platform::noa_gpui_scene::Point::new(
            r.x + r.width / 2.0,
            r.y + r.height / 2.0,
        );
        let button = PointerButton::Primary;
        f.tree
            .dispatch(Event::PointerDown { pos, button }, &mut f.text);
        f.tree
            .dispatch(Event::PointerUp { pos, button }, &mut f.text);
        f.tree.take_messages()
    }

    /// Clicks `id` and hands the tree's messages to the volumes page.
    fn click_volumes(f: &mut Fixture, view: &mut VolumesView, id: WidgetId) {
        for m in click(f, id) {
            view.handle(&m, &mut f.cx());
        }
    }

    /// The one action the open confirmation would run.
    fn confirmed(f: &Fixture) -> Act {
        let acts: Vec<_> = f.alerts.values().flatten().flatten().cloned().collect();
        assert_eq!(acts.len(), 1, "{acts:?}");
        acts[0].clone()
    }

    fn network(id: &str, builtin: bool) -> crate::dto::Network {
        crate::dto::Network {
            id: id.into(),
            name: id.into(),
            driver: "bridge".into(),
            scope: "local".into(),
            internal: false,
            builtin,
        }
    }

    /// "Select all" on networks selects the built-in ones too, but only the others are
    /// counted and removed.
    #[test]
    fn networks_remove_the_selection_without_builtin_ones() {
        let mut f = Fixture::new();
        f.set_connected(true);
        f.snap.networks = vec![
            network("bridge", true),
            network("app", false),
            network("db", false),
        ];
        let root = f.root;
        let mut view = NetworksView::build(root, &mut f.cx());
        view.refresh(&mut f.cx());
        for m in click(&mut f, view.bulk.select_all) {
            view.handle(&m, &mut f.cx());
        }
        assert_eq!(button_label(&f, view.bulk.remove), "Remove selected (2)");
        for m in click(&mut f, view.bulk.remove) {
            view.handle(&m, &mut f.cx());
        }
        let Act::RemoveNetworks(ids) = confirmed(&f) else {
            panic!("not a network removal")
        };
        assert_eq!(ids, ["app", "db"]);
    }

    /// Selected images are removed together; one used by a container is forced.
    #[test]
    fn images_remove_the_selection_forcing_used_ones() {
        let mut f = Fixture::new();
        f.set_connected(true);
        let image = |id: &str, containers: f64| crate::dto::Image {
            id: id.into(),
            short_id: id.into(),
            tags: vec![format!("{id}:latest")],
            size: 1.0,
            created: 0.0,
            containers,
            dangling: false,
        };
        f.snap.images = vec![image("used", 1.0), image("free", 0.0)];
        let root = f.root;
        let mut view = ImagesView::build(root, &mut f.cx());
        view.refresh(&mut f.cx());
        for m in click(&mut f, view.bulk.select_all) {
            view.handle(&m, &mut f.cx());
        }
        for m in click(&mut f, view.bulk.remove) {
            view.handle(&m, &mut f.cx());
        }
        let Act::RemoveImages(mut images) = confirmed(&f) else {
            panic!("not an image removal")
        };
        images.sort();
        assert_eq!(
            images,
            [("free".to_owned(), false), ("used".to_owned(), true)]
        );
    }

    /// Selected containers are removed together; running ones are forced.
    #[test]
    fn containers_remove_the_selection_forcing_running_ones() {
        let mut f = Fixture::new();
        f.set_connected(true);
        let mut stopped = running(ContainerState::Exited);
        stopped.id = "c2".into();
        stopped.name = "old".into();
        f.snap.containers = vec![running(ContainerState::Running), stopped];
        f.config.show_stopped = true;
        let root = f.root;
        let mut view = ContainersView::build(root, BTreeSet::new(), &mut f.cx());
        view.refresh(&mut f.cx());
        for m in click(&mut f, view.bulk.select_all) {
            view.handle(&m, &mut f.cx());
        }
        assert_eq!(button_label(&f, view.bulk.remove), "Remove selected (2)");
        for m in click(&mut f, view.bulk.remove) {
            view.handle(&m, &mut f.cx());
        }
        let Act::RemoveContainers(mut cs) = confirmed(&f) else {
            panic!("not a container removal")
        };
        cs.sort();
        assert_eq!(cs, [("c1".to_owned(), true), ("c2".to_owned(), false)]);
    }

    fn button_label(f: &Fixture, id: WidgetId) -> String {
        f.tree.get::<Button>(id).unwrap().label().to_owned()
    }

    /// "Select all" selects every volume (and then deselects them), and "Remove
    /// selected" asks once for all the selected volumes, counting the mounted ones.
    #[test]
    fn volumes_select_all_and_remove_the_selection() {
        let mut f = Fixture::new();
        f.set_connected(true);
        f.snap.volumes = vec![volume("b"), volume("a"), volume("c")];
        let mut mounted = running(ContainerState::Running);
        mounted.volumes = vec!["a".into()];
        f.snap.containers = vec![mounted];
        let root = f.root;
        let mut view = VolumesView::build(root, &mut f.cx());
        view.refresh(&mut f.cx());
        assert!(!f.tree.is_enabled(view.bulk.remove));
        assert_eq!(button_label(&f, view.bulk.remove), "Remove selected (0)");

        let select_all = view.bulk.select_all;
        click_volumes(&mut f, &mut view, select_all);
        assert_eq!(view.list.table.selected_keys(&f.tree), ["a", "b", "c"]);
        assert_eq!(button_label(&f, view.bulk.select_all), "Deselect all");
        assert_eq!(button_label(&f, view.bulk.remove), "Remove selected (3)");
        assert!(f.tree.is_enabled(view.bulk.remove));

        click_volumes(&mut f, &mut view, select_all);
        assert!(view.list.table.selected_keys(&f.tree).is_empty());
        assert_eq!(button_label(&f, view.bulk.select_all), "Select all");
        click_volumes(&mut f, &mut view, select_all);

        // A volume removed elsewhere drops out of the selection.
        f.snap.volumes.retain(|v| v.name != "c");
        view.refresh(&mut f.cx());
        assert_eq!(button_label(&f, view.bulk.remove), "Remove selected (2)");

        let remove = view.bulk.remove;
        click_volumes(&mut f, &mut view, remove);
        let acts: Vec<_> = f.alerts.values().flatten().flatten().collect();
        assert!(
            matches!(acts.as_slice(), [Act::RemoveVolumes(names)] if names == &["a", "b"]),
            "{acts:?}"
        );
    }

    fn select(f: &mut Fixture, grid: &mut Grid, rows: &[usize]) {
        let rows = rows.to_vec();
        f.tree
            .update::<TableView, _>(grid.id, |t, ucx| t.select_rows(ucx, rows));
        grid.sync_selection(&f.tree);
    }

    fn escape() -> Shortcut {
        Shortcut::key(Key::Escape)
    }

    fn delete() -> Shortcut {
        Shortcut::key(Key::Delete)
    }

    /// Delete asks before removing one volume (by name) or the selection; Esc
    /// clears the selection; nothing is asked while disconnected.
    #[test]
    fn volumes_take_delete_and_escape_like_containers() {
        let mut f = Fixture::new();
        f.set_connected(true);
        f.snap.volumes = vec![volume("a"), volume("b")];
        let root = f.root;
        let mut view = VolumesView::build(root, &mut f.cx());
        view.refresh(&mut f.cx());

        view.shortcut(delete(), &mut f.cx());
        assert!(f.alerts.is_empty(), "nothing selected");

        select(&mut f, &mut view.list.table, &[1]);
        view.shortcut(delete(), &mut f.cx());
        assert!(matches!(confirmed(&f), Act::RemoveVolume(name) if name == "b"));
        f.alerts.clear();

        select(&mut f, &mut view.list.table, &[0, 1]);
        view.shortcut(delete(), &mut f.cx());
        assert!(matches!(confirmed(&f), Act::RemoveVolumes(names) if names == ["a", "b"]));
        f.alerts.clear();

        f.set_connected(false);
        view.shortcut(delete(), &mut f.cx());
        assert!(f.alerts.is_empty(), "disconnected");

        view.inspect.inspecting = Some("a".into());
        f.tree.set_hidden(view.inspect.root, false);
        view.shortcut(escape(), &mut f.cx());
        assert!(f.tree.is_hidden(view.inspect.root));
        assert!(view.list.table.selected_keys(&f.tree).is_empty());
        assert_eq!(view.list.selected(), None);
        assert_eq!(button_label(&f, view.bulk.remove), "Remove selected (0)");
    }

    /// Esc also closes the inspect pane (Volumes and Networks do the same).
    #[test]
    fn images_take_delete_and_escape_like_containers() {
        let mut f = Fixture::new();
        f.set_connected(true);
        let image = |id: &str, containers: f64| crate::dto::Image {
            id: id.into(),
            short_id: id.into(),
            tags: vec![format!("{id}:latest")],
            size: 1.0,
            created: 0.0,
            containers,
            dangling: false,
        };
        f.snap.images = vec![image("used", 1.0), image("free", 0.0)];
        let root = f.root;
        let mut view = ImagesView::build(root, &mut f.cx());
        view.refresh(&mut f.cx());

        select(&mut f, &mut view.list.table, &[0]);
        view.shortcut(delete(), &mut f.cx());
        assert!(matches!(confirmed(&f), Act::RemoveImage(id, true) if id == "used"));
        f.alerts.clear();

        // As the inspect button leaves it (without the request it sends).
        view.inspect.inspecting = Some("free".into());
        f.tree.set_hidden(view.inspect.root, false);
        view.shortcut(escape(), &mut f.cx());
        assert!(f.tree.is_hidden(view.inspect.root));
        assert!(view.inspect.inspecting.is_none());
        assert!(view.list.table.selected_keys(&f.tree).is_empty());
    }

    /// A built-in network is not offered for removal by Delete either.
    #[test]
    fn networks_take_delete_without_builtin_ones() {
        let mut f = Fixture::new();
        f.set_connected(true);
        f.snap.networks = vec![network("bridge", true), network("app", false)];
        let root = f.root;
        let mut view = NetworksView::build(root, &mut f.cx());
        view.refresh(&mut f.cx());
        // Sorted by name: app, bridge.
        select(&mut f, &mut view.list.table, &[1]);
        view.shortcut(delete(), &mut f.cx());
        assert!(f.alerts.is_empty(), "built-in");

        select(&mut f, &mut view.list.table, &[0]);
        view.shortcut(delete(), &mut f.cx());
        assert!(matches!(confirmed(&f), Act::RemoveNetwork(id) if id == "app"));
        f.alerts.clear();

        select(&mut f, &mut view.list.table, &[0, 1]);
        view.shortcut(delete(), &mut f.cx());
        assert!(matches!(confirmed(&f), Act::RemoveNetworks(ids) if ids == ["app"]));
    }

    /// "Start selected" and "Stop selected" count only the selected containers each can
    /// act on, and stay disabled while none can.
    #[test]
    fn containers_start_and_stop_count_the_selection_they_act_on() {
        let mut f = Fixture::new();
        f.set_connected(true);
        let mut stopped = running(ContainerState::Exited);
        stopped.id = "c2".into();
        stopped.name = "old".into();
        f.snap.containers = vec![running(ContainerState::Running), stopped];
        f.config.show_stopped = true;
        let root = f.root;
        let mut view = ContainersView::build(root, BTreeSet::new(), &mut f.cx());
        view.refresh(&mut f.cx());
        assert_eq!(button_label(&f, view.start_selected), "Start selected (0)");
        assert!(!f.tree.is_enabled(view.start_selected));
        assert!(!f.tree.is_enabled(view.stop_selected));
        for m in click(&mut f, view.bulk.select_all) {
            view.handle(&m, &mut f.cx());
        }
        assert_eq!(button_label(&f, view.start_selected), "Start selected (1)");
        assert_eq!(button_label(&f, view.stop_selected), "Stop selected (1)");
        assert!(f.tree.is_enabled(view.start_selected));
        assert!(f.tree.is_enabled(view.stop_selected));
    }

    /// Duplicate opens the Run form on the containers page, filled in, in place of the
    /// detail pane; a selection or Esc gives the side back.
    #[test]
    fn duplicate_fills_the_run_form_in_place_of_the_detail_pane() {
        let mut f = Fixture::new();
        f.set_connected(true);
        f.snap.containers = vec![running(ContainerState::Running)];
        let root = f.root;
        let mut view = ContainersView::build(root, BTreeSet::new(), &mut f.cx());
        view.refresh(&mut f.cx());
        select(&mut f, &mut view.table, &[0]);
        view.refresh(&mut f.cx());
        assert!(!f.tree.is_hidden(view.detail.root));
        assert!(f.tree.is_enabled(view.detail.duplicate));

        let template = RunTemplate {
            source: "web".into(),
            form: RunForm {
                image: "nginx".into(),
                name: "web-copy".into(),
                command: "nginx -g 'daemon off;'".into(),
                ports: "8080:80".into(),
                env: "A=1".into(),
                volumes: "data:/data".into(),
                network: "shop_default".into(),
                restart: RestartChoice::UnlessStopped,
            },
            skipped: Vec::new(),
        };
        view.open_template(Ok(template.clone()), &mut f.cx());
        assert!(!f.tree.is_hidden(view.run_pane.root));
        assert!(f.tree.is_hidden(view.detail.root));
        assert_eq!(view.run_pane.form(&f.tree), template.form);
        assert_eq!(view.run_pane.restart_choice, RestartChoice::UnlessStopped);

        // A request that failed leaves the side as it is.
        view.open_template(
            Err(UiError {
                kind: crate::dto::UiErrorKind::Local,
                message: "gone".into(),
            }),
            &mut f.cx(),
        );
        assert!(!f.tree.is_hidden(view.run_pane.root));

        view.shortcut(escape(), &mut f.cx());
        assert!(f.tree.is_hidden(view.run_pane.root));

        view.open_template(Ok(template), &mut f.cx());
        select(&mut f, &mut view.table, &[0]);
        view.refresh(&mut f.cx());
        assert!(f.tree.is_hidden(view.run_pane.root));
        assert!(!f.tree.is_hidden(view.detail.root));
    }

    /// A check for newer images marks the rows that have one and turns its button
    /// back on; a failed check leaves the marks as they were.
    #[test]
    fn a_check_for_updates_marks_the_rows_with_a_newer_image() {
        let mut f = Fixture::new();
        f.set_connected(true);
        let mut other = running(ContainerState::Exited);
        other.id = "c2".into();
        other.name = "db".into();
        f.snap.containers = vec![running(ContainerState::Running), other];
        let root = f.root;
        let mut view = ContainersView::build(root, BTreeSet::new(), &mut f.cx());
        view.refresh(&mut f.cx());
        assert!(f.tree.is_enabled(view.check_updates));
        let note = |view: &ContainersView, key: &str| {
            let row = view.table.rows.iter().find(|r| r.key == key).unwrap();
            row.details.get(3).cloned().flatten()
        };
        assert_eq!(note(&view, "c1"), None);

        view.checking = true;
        view.refresh(&mut f.cx());
        assert!(
            !f.tree.is_enabled(view.check_updates),
            "one check at a time"
        );

        let t = Tr(Lang::En);
        let checks = vec![
            UpdateCheck {
                container: "c1".into(),
                freshness: Freshness::Available,
            },
            UpdateCheck {
                container: "c2".into(),
                freshness: Freshness::Current,
            },
        ];
        assert!(f.tree.is_hidden(view.update_all));
        view.show_update_checks(Ok(checks), &mut f.cx());
        assert!(f.tree.is_enabled(view.check_updates));
        assert_eq!(note(&view, "c1").as_deref(), Some(t.update_available()));
        assert_eq!(note(&view, "c2"), None);

        // "Update all" offers the one container with a newer image.
        assert!(!f.tree.is_hidden(view.update_all));
        assert_eq!(button_label(&f, view.update_all), t.update_all(1));
        for m in click(&mut f, view.update_all) {
            view.handle(&m, &mut f.cx());
        }
        let Act::UpdateContainers(targets) = confirmed(&f) else {
            panic!("not an update")
        };
        assert_eq!(targets, [("c1".to_owned(), "web".to_owned())]);
        f.alerts.clear();

        view.show_update_checks(
            Err(UiError {
                kind: crate::dto::UiErrorKind::Transport,
                message: "gone".into(),
            }),
            &mut f.cx(),
        );
        assert_eq!(note(&view, "c1").as_deref(), Some(t.update_available()));

        // Once it is replaced (a new id), there is nothing left to update.
        f.snap.containers[0].id = "c1-new".into();
        view.refresh(&mut f.cx());
        assert!(f.tree.is_hidden(view.update_all));
        assert_eq!(note(&view, "c1-new"), None);
    }

    /// Networks… offers to join each network the container is not on and to leave
    /// each it is on, one button each, and Cancel.
    #[test]
    fn networks_offers_a_button_per_network_change() {
        let mut f = Fixture::new();
        f.set_connected(true);
        let mut web = running(ContainerState::Running);
        web.networks = vec!["front".into()];
        f.snap.containers = vec![web];
        f.snap.networks = vec![network("front", false), network("back", false)];
        let root = f.root;
        let mut view = ContainersView::build(root, BTreeSet::new(), &mut f.cx());
        view.refresh(&mut f.cx());
        select(&mut f, &mut view.table, &[0]);
        view.refresh(&mut f.cx());
        assert!(f.tree.is_enabled(view.detail.networks));
        for m in click(&mut f, view.detail.networks) {
            view.handle(&m, &mut f.cx());
        }
        let acts: Vec<Option<Act>> = f.alerts.values().flatten().cloned().collect();
        assert_eq!(acts.len(), 3, "{acts:?}");
        assert!(matches!(
            &acts[0],
            Some(Act::ContainerNetwork(id, n, true)) if id == "c1" && n == "back"
        ));
        assert!(matches!(
            &acts[1],
            Some(Act::ContainerNetwork(id, n, false)) if id == "c1" && n == "front"
        ));
        assert!(acts[2].is_none(), "Cancel");

        // On the host's network nothing can change: no dialog.
        f.alerts.clear();
        f.snap.containers[0].networks = vec!["host".into()];
        view.refresh(&mut f.cx());
        for m in click(&mut f, view.detail.networks) {
            view.handle(&m, &mut f.cx());
        }
        assert!(f.alerts.is_empty());
    }

    /// Update asks first and names the container; a stack's container is pointed at the
    /// stack's Pull and Up instead.
    #[test]
    fn update_asks_before_replacing_and_leaves_stacks_to_compose() {
        let mut f = Fixture::new();
        f.set_connected(true);
        f.snap.containers = vec![running(ContainerState::Running)];
        let root = f.root;
        let mut view = ContainersView::build(root, BTreeSet::new(), &mut f.cx());
        view.refresh(&mut f.cx());
        select(&mut f, &mut view.table, &[0]);
        view.refresh(&mut f.cx());
        assert!(f.tree.is_enabled(view.detail.update));
        for m in click(&mut f, view.detail.update) {
            view.handle(&m, &mut f.cx());
        }
        let Act::UpdateContainer(id, name) = confirmed(&f) else {
            panic!("not an update")
        };
        assert_eq!((id.as_str(), name.as_str()), ("c1", "web"));

        f.alerts.clear();
        f.snap.containers[0].compose = Some(crate::dto::ComposeLabels {
            project: "shop".into(),
            service: "web".into(),
        });
        view.refresh(&mut f.cx());
        for m in click(&mut f, view.detail.update) {
            view.handle(&m, &mut f.cx());
        }
        assert!(f.alerts.is_empty(), "a stack's container is not replaced");

        f.set_connected(false);
        view.refresh(&mut f.cx());
        assert!(!f.tree.is_enabled(view.detail.update));
    }

    /// The Run form keeps what was typed for the same image (to fix a refused run) and
    /// starts empty for another one.
    #[test]
    fn run_form_reads_its_fields_and_resets_for_another_image() {
        let mut f = Fixture::new();
        f.set_connected(true);
        let root = f.root;
        let mut view = ImagesView::build(root, &mut f.cx());
        view.refresh(&mut f.cx());
        view.run_pane.open("nginx:1.27", &mut f.cx());
        assert!(!f.tree.is_hidden(view.run_pane.root));
        let pane = &view.run_pane;
        for (id, text) in [
            (pane.name, "web"),
            (pane.ports, "8080:80"),
            (pane.volumes, "data:/data"),
        ] {
            f.tree
                .update::<TextField, _>(id, |t, ucx| t.set_text(ucx, text));
        }
        f.tree
            .update::<TextView, _>(pane.env, |v, ucx| v.set_text(ucx, "A=1"));
        view.run_pane.restart_choice = RestartChoice::Always;
        let form = view.run_pane.form(&f.tree);
        assert_eq!(form.image, "nginx:1.27");
        assert_eq!(form.name, "web");
        assert_eq!(form.ports, "8080:80");
        assert_eq!(form.volumes, "data:/data");
        assert_eq!(form.env, "A=1");
        assert_eq!(form.restart, RestartChoice::Always);

        view.run_pane.hide(&mut f.cx());
        view.run_pane.open("nginx:1.27", &mut f.cx());
        assert_eq!(view.run_pane.form(&f.tree).name, "web");

        view.run_pane.open("redis:7", &mut f.cx());
        let form = view.run_pane.form(&f.tree);
        assert_eq!(form.image, "redis:7");
        assert_eq!((form.name.as_str(), form.env.as_str()), ("", ""));
        assert_eq!(form.restart, RestartChoice::No);
    }

    /// An inspect reply shows only while its volume is the one in the pane.
    #[test]
    fn inspect_pane_shows_only_the_reply_it_waits_for() {
        let mut f = Fixture::new();
        f.set_connected(true);
        f.snap.volumes = vec![volume("a")];
        let root = f.root;
        let mut view = VolumesView::build(root, &mut f.cx());
        view.refresh(&mut f.cx());
        view.inspect.inspecting = Some("a".into());
        let text = |f: &Fixture| {
            f.tree
                .get::<TextView>(view.inspect.text)
                .unwrap()
                .text()
                .to_owned()
        };
        view.inspect
            .show("b", Ok("{\"Name\": \"b\"}".into()), &mut f.cx());
        assert_eq!(text(&f), "");
        view.inspect
            .show("a", Ok("{\"Name\": \"a\"}".into()), &mut f.cx());
        assert_eq!(text(&f), "{\"Name\": \"a\"}");
    }

    /// Parts of a page start hidden (detail pane, compose output, inspect pane).
    /// Showing them must lay out and paint without panicking (noa-gpui's
    /// SegmentedControl once did when painted before its first measure).
    #[test]
    fn every_page_paints_with_its_hidden_parts_shown() {
        let target = engine::ConnectTarget {
            host: "unix:///nonexistent.sock".into(),
            source: "test".into(),
            context: None,
            tls: None,
        };
        let (backend, _rx) =
            Backend::new(AppState::new(target), ConfigStore::new(Config::default()));
        let snap = backend.snapshot();
        let config = Config::default();
        let viewport = Size::new(1200.0, 800.0);
        for page in [
            Page::Containers,
            Page::Images,
            Page::Volumes,
            Page::Networks,
            Page::Settings,
        ] {
            let mut text = TextStore::new(FontOptions::default());
            let mut tree = UiTree::new();
            let root = tree.insert(None, widgets::fill_column(tokens::SPACE_3, 0.0));
            let (mut alerts, mut save, mut open) = (HashMap::new(), None, false);
            let mut cx = Ctx {
                tree: &mut tree,
                text: &mut text,
                backend: &backend,
                snap: &snap,
                config: &config,
                t: Tr(Lang::En),
                alerts: &mut alerts,
                save_config: &mut save,
                open_contexts: &mut open,
            };
            let mut view = PageView::build(page, root, BTreeSet::new(), &mut cx);
            view.refresh(&mut cx);
            tree.prepare(viewport, &mut text);
            let mut ids = Vec::new();
            subtree(&tree, root, &mut ids);
            for id in ids {
                tree.set_hidden(id, false);
            }
            tree.prepare(viewport, &mut text);
        }
    }
}

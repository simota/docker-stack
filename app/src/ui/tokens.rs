//! Design tokens: the one scale every gap, padding, text size, weight and line in the
//! window is taken from (the scale of the earlier web UI).

// ---- space (gaps and padding) ----

/// Between the lines of one item (a name over its address).
pub const SPACE_HALF: f32 = 2.0;
pub const SPACE_1: f32 = 4.0;
pub const SPACE_2: f32 = 6.0;
pub const SPACE_3: f32 = 8.0;
pub const SPACE_4: f32 = 12.0;
pub const SPACE_5: f32 = 16.0;
pub const SPACE_6: f32 = 24.0;

/// Side and vertical padding of a page's header, toolbars and notes.
pub const PAGE_X: f32 = SPACE_5;
pub const PAGE_Y: f32 = SPACE_3;
/// A list's inset from the window's sides: with the table's 8 px of cell padding,
/// the cells' text lines up with the page's margin.
pub const LIST_INSET: f32 = PAGE_X - CELL_PAD;
/// Space between a table cell's edges and its text.
pub const CELL_PAD: f32 = SPACE_3;

// ---- text ----

pub const TEXT_XS: f32 = 11.0;
pub const TEXT_SM: f32 = 12.0;
pub const TEXT_BASE: f32 = 13.0;
pub const TEXT_MD: f32 = 15.0;
pub const TEXT_LG: f32 = 17.0;

pub const WEIGHT_STRONG: u16 = 600;

// ---- shape ----

/// A hairline between rows and sections, and the heavier line that opens a block.
pub const RULE: f32 = 1.0;
pub const RULE_STRONG: f32 = 2.0;

/// The status dot before the connection.
pub const DOT: f32 = 8.0;
/// A usage meter's bar.
pub const METER_BAR: f32 = 3.0;

// ---- sizes ----

/// Width of a usage meter in the command deck (narrow enough for the deck's first
/// row to fit the default window).
pub const METER_WIDTH: f32 = 96.0;
/// Widths of text fields: search, pull reference, new resource name, a number.
pub const FIELD_SEARCH: f32 = 220.0;
pub const FIELD_WIDE: f32 = 260.0;
pub const FIELD_NAME: f32 = 200.0;
pub const FIELD_NUMBER: f32 = 120.0;
/// Width of the label column of a list of facts.
pub const FACT_LABEL: f32 = 120.0;
/// Width of the label before a setting's control, so the controls line up.
pub const SETTING_LABEL: f32 = 80.0;
/// Width of a settings column.
pub const SETTINGS_COLUMN: f32 = 480.0;
/// Width of a popover's content (disk usage, contexts).
pub const POPOVER_WIDTH: f32 = 380.0;
/// The disk usage popover: the kind's label column and the bar's height.
pub const DISK_LABEL: f32 = 88.0;
pub const DISK_BAR: f32 = 6.0;
/// The disk usage popover's size and share columns.
pub const DISK_SIZE: f32 = 72.0;
pub const DISK_SHARE: f32 = 40.0;
/// The detail pane beside a list: share of the width, and its minimum.
pub const PANE_SHARE: f32 = 0.46;
pub const PANE_MIN: f32 = 380.0;

/// Height of a row in the lists, of an icon button in a row (it is as wide), and of
/// a stack row's buttons.
pub const LIST_ROW: f32 = 44.0;
pub const ICON_BUTTON: f32 = 24.0;
pub const GROUP_BUTTON: f32 = 22.0;

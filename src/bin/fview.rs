//! Optional iced-based GUI: grep a big CSV file and browse the matching rows.
//!
//! This binary is only built when the `gui` feature is enabled, so the normal
//! `fvalidate` tool keeps compiling with no GUI dependency:
//!
//! ```text
//! cargo run --release --features gui --bin fview
//! cargo run --release --features gui --bin fview -- data.csv
//! cargo run --release --features gui --bin fview -- data.csv -d '\t' --case-sensitive
//! ```
//!
//! The file is optional: with no path the window opens on a welcome screen with
//! an **Open CSV…** button that opens a native file picker (`rfd`).
//!
//! Layout:
//! * a flat menu bar holds the brand, the Open/Rules/Table/Index tabs and the
//!   file badge;
//! * the bar below it holds the regex filter and the search button;
//! * a docking panel on the right holds the configuration: the scan/view
//!   options, the attribute filter and, when attributes are hidden, a chip per
//!   hidden attribute (click a chip to show the attribute again). Hidden chips
//!   are sorted alphabetically so large attribute lists stay navigable, and the
//!   attribute filter narrows that list (and highlights the matching chips in
//!   the main view);
//! * every matching row is rendered as a set of `attribute = value` chips, each
//!   with a mute icon that hides that attribute from all rows and moves its name
//!   into the top bar, and a lock icon that pins the attribute so it always
//!   stays visible (mute, mute-all, profiles and "rule attributes only" all
//!   leave locked columns alone).
//!
//! Scanning: the filter is **debounced** (a scan starts ~180 ms after the last
//! keystroke, and an unchanged pattern is never re-scanned). The file is
//! memory-mapped read-only. Two opt-in checkboxes in the configuration panel
//! change the scan: **visible only** searches just the attributes that are
//! currently
//! shown, and **parallel** reads the whole file in record-aligned segments
//! across all cores, which yields exact row/match totals but never exits early.
//!
//! Display and indexing: a **table** checkbox renders the matches as a table of
//! the visible attributes instead of chips. Each chip, and each table header,
//! carries a database button that builds (or drops) a per-column prefix
//! **index**, a mute button that hides the attribute and a lock button that
//! pins it visible; indexed attributes are
//! highlighted (green background, filled icon) in both views. While an index
//! exists and the **index** checkbox is on, a non-empty filter becomes a
//! case-insensitive `beginsWith` prefix query over the indexed columns — served
//! straight from the index, with exact totals and no file scan. Unchecking
//! **index** (or using `--case-sensitive`) falls back to the regex. Chips show
//! only the cell value by default; the **attribute names** checkbox brings back
//! the `attribute = value` label. Clicking a chip copies its value (a tooltip
//! reveals values clipped by the two-line limit, and a double click opens the
//! row form), and clicking a table row — or the empty part of a chip row —
//! opens the form directly. The form closes
//! with its button, the Escape key, or a click on the backdrop. Escape also
//! clears the regex or attribute filter the user was last editing.
//!
//! Profiles: the set of currently visible attributes can be saved under a name
//! and re-applied later. Profiles are persisted as TOML in the platform config
//! directory (`<config>/fview/profiles.toml`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use fast_csv::dsl;
use fast_csv::engine::{self, EngineConfig};
use fast_csv::report::{Report, RowHit, RowOutcome, RuleReport};
use fast_csv::rules::{self, Plan};
use iced::keyboard::{self, Key};
use iced::widget::scrollable::AbsoluteOffset;
use iced::widget::text::Wrapping;
use iced::widget::{
    button, checkbox, column, container, horizontal_rule, mouse_area, opaque, pick_list, row,
    scrollable, stack, text, text_input, tooltip, Row, Space,
};
use iced::theme::Palette;
use iced::{
    Background, Border, Center, Color, Element, Fill, Length, Padding, Shadow, Subscription, Task,
    Theme, Vector,
};
use iced_fonts::{Bootstrap, BOOTSTRAP_FONT, BOOTSTRAP_FONT_BYTES};
use memmap2::Mmap;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use simd_csv::ByteRecord;

const BUFFER_CAPACITY: usize = 64 * 1024;
/// How often the debounce timer is polled while a filter edit is pending.
const DEBOUNCE_TICK_MS: u64 = 50;
/// Quiet period after the last filter keystroke before a scan is started.
const DEBOUNCE_QUIET_MS: u64 = 180;
/// Fixed height of one table row in the table view.
const TABLE_ROW_HEIGHT: f32 = 24.0;
/// Height of a distinct-condition group heading in the grid.
const GROUP_HEADING_HEIGHT: f32 = 26.0;
/// Minimum width of a table column. The table grows horizontally instead of
/// squeezing columns below this.
const TABLE_CELL_MIN_WIDTH: f32 = 160.0;
/// Horizontal room a table header needs besides its label: container padding
/// plus the database / mute / lock icon column. Folded into the column width so
/// a long header is never squeezed into wrapping.
const TABLE_HEADER_CHROME: f32 = 88.0;
/// Spacing between chips, in px.
const CHIP_SPACING: f32 = 8.0;
/// Non-text width of a chip: padding + index icon + mute icon + lock icon +
/// inner spacing.
const CHIP_CHROME: f32 = 90.0;
/// Non-text height of a chip: vertical padding + border.
const CHIP_CHROME_V: f32 = 8.0;
/// Height of a single line of chip text.
const CHIP_LINE_HEIGHT: f32 = 16.0;
/// A chip may wrap to at most this many lines; longer values are clipped.
/// Capping the height is what lets every data stripe have a fixed height, which
/// in turn makes the virtual scrolling below exact.
const MAX_CHIP_LINES: usize = 2;
/// Vertical padding of a data stripe (kept in sync with `container.padding`).
const STRIPE_PADDING: f32 = 8.0;
/// Horizontal padding of a data stripe (kept in sync with `container.padding`).
const STRIPE_PADDING_H: f32 = 12.0;
/// Padding of the content area that holds the rules sidebar and the grid.
const CONTENT_PADDING: f32 = 10.0;
/// Gap between the docked rules sidebar and the grid.
const MAIN_GAP: f32 = 8.0;
/// Right padding of the row list. It keeps chips clear of the scrollbar iced
/// draws over the scrollable's right edge.
const LIST_RIGHT_PADDING: f32 = 14.0;
/// Inner padding of the grid card. It keeps the opaque striped rows off the
/// rounded border, so the card's corner radius stays visible instead of being
/// covered by a square stripe. Must clear the border's inner arc, i.e. at least
/// `CARD_RADIUS - (CARD_RADIUS - 1) / √2` (~3.64 for radius 10), so a row
/// corner sits inside the rounded corner rather than over it.
const GRID_PADDING: f32 = 5.0;
/// Spacing between the chip lines inside a stripe.
const CHIP_LINE_SPACING: f32 = 6.0;
/// Rows rendered above and below the viewport so scrolling does not flash gaps.
const OVERSCAN_ROWS: usize = 3;
/// Rough width of one character at size 13, used to decide text wrapping.
const CHAR_WIDTH: f32 = 7.2;
/// Corner radius shared by cards, inputs and buttons.
const RADIUS: f32 = 6.0;
/// Corner radius of the large floating panels.
const CARD_RADIUS: f32 = 10.0;
/// Height reserved for the status line. Fixed so the different states (plain
/// text, the taller icon + "scanning…" row) do not nudge the rows below it.
const STATUS_HEIGHT: f32 = 22.0;
/// Row-count choices offered by the "rows" drop-down. The largest is bounded so
/// the grid never tries to hold an unbounded list.
const ROW_LIMIT_CHOICES: [usize; 6] = [100, 250, 500, 1000, 5000, 10000];

#[derive(Parser, Debug, Clone)]
#[command(name = "fview", about = "Grep and browse rows of a big CSV file (GUI)")]
struct Args {
    /// CSV file to view. Optional: without it, use the Open button.
    path: Option<PathBuf>,

    /// Field delimiter (single byte; use '\t' for a tab).
    #[arg(short = 'd', long, default_value = ",")]
    delimiter: String,

    /// Make the filter regex case-sensitive (case-insensitive by default).
    #[arg(short = 's', long, default_value_t = false)]
    case_sensitive: bool,

    /// Maximum number of matching rows to display (default 100).
    #[arg(short = 'n', long, default_value_t = 100)]
    limit: usize,

    /// Rendering backend. `auto` prefers the GPU (wgpu) and falls back to the
    /// CPU renderer (tiny-skia) when no GPU is available; the other values
    /// force a specific backend.
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    backend: Backend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// Prefer the GPU, fall back to the CPU renderer.
    Auto,
    /// Force the wgpu (GPU) renderer.
    Wgpu,
    /// Force the tiny-skia (CPU) renderer.
    TinySkia,
}

#[derive(Debug, Clone)]
struct ScanResult {
    rows: Vec<Vec<String>>,
    /// Best known number of matching data rows. Exact after a full (parallel)
    /// scan; after an early-exiting sequential scan this is the number of kept
    /// rows, since the true total was never read.
    matched: usize,
    truncated: bool,
    /// Number of data rows actually read from the file. When `truncated` is
    /// false this is the total number of rows in the file.
    rows_read: usize,
    /// True when the matches came from a built index (prefix search) rather
    /// than a file scan.
    indexed: bool,
}

impl ScanResult {
    fn empty() -> Self {
        ScanResult {
            rows: Vec::new(),
            matched: 0,
            truncated: false,
            rows_read: 0,
            indexed: false,
        }
    }
}

/// State of the rule-evaluation panel: which rule file is loaded, the compiled
/// program, the evaluated report (if any) and a bounded cache of the full row
/// lists collected for individual rules.
#[derive(Debug, Default)]
struct RulesState {
    /// Rules DSL file chosen by the user.
    path: Option<PathBuf>,
    /// Result of the last evaluation, kept in memory for the panel.
    report: Option<Report>,
    /// The compiled rules program for the current CSV. Kept so collecting one
    /// rule's rows never re-reads or recompiles the rules file.
    plan: Option<Arc<Plan>>,
    error: Option<String>,
    evaluating: bool,
    /// Complete row list of one rule (every outcome), with the full CSV row for
    /// each hit. It feeds the main grid when a rule filter is active. Only one
    /// rule's rows are ever kept: collecting another rule drops them so the
    /// process never holds several multi-million-row lists at once.
    hits: Option<RuleHits>,
    /// Which side of `hits` the grid shows: `None` = every row, otherwise only
    /// rows with that outcome (passed, failed, skipped, validation-skipped).
    hits_filter: Option<RowOutcome>,
    /// When true, a collected rule's rows are sampled per distinct
    /// `(left, right, expected)` condition instead of filling the budget with
    /// rows that all failed the same way. The grid then groups rows by
    /// condition.
    distinct: bool,
    /// Whether the main grid currently shows rule rows (rather than scan
    /// results).
    view_active: bool,
    /// Rule whose rows are being collected for the grid.
    pending_rule: Option<usize>,
    collecting: bool,
    /// Latest rule the user asked for while another collection was running.
    /// Started once the in-flight pass finishes, so rapid clicks never launch
    /// several whole-file evaluations at the same time.
    queued_rule: Option<(usize, Option<RowOutcome>)>,
    /// Restrict the main grid (and the detail form) to the attributes the
    /// active rule references.
    attrs_only: bool,
    /// The user's hidden-attribute set before `attrs_only` narrowed the grid, so
    /// it can be restored when the mode or the rule view ends.
    saved_muted: Option<HashSet<usize>>,
    /// Bumped on every evaluation so stale background results are dropped.
    generation: u64,
    /// When the current rule-row collection started, and how long it took. The
    /// status line shows the duration next to the row count, like a scan.
    collect_started: Option<Instant>,
    collect_duration: Option<Duration>,
}

/// The compiled rules program and its evaluation result, returned together so
/// the panel can cache the program and re-evaluate a single rule later without
/// reloading the DSL file.
#[derive(Debug, Clone)]
struct EvaluatedRules {
    plan: Arc<Plan>,
    report: Report,
}

/// Every row of one rule (passed, failed, skipped or validation-skipped),
/// together with the full CSV row for each hit (in `hits` order) so the main
/// grid can render the attributes. The retained list is capped per outcome, so
/// `passed`/`failed`/... carry the exact totals from the full evaluation.
#[derive(Debug, Clone)]
struct RuleHits {
    rule: usize,
    /// Per-outcome cap used when collecting. A larger display limit needs a
    /// fresh collection.
    cap: usize,
    /// Whether the rows were collected per distinct condition.
    distinct: bool,
    hits: Vec<RowHit>,
    /// Shared with the grid: switching outcome clones the `Arc`s, not the rows.
    rows: Vec<Arc<Vec<String>>>,
    passed: u64,
    failed: u64,
    skipped: u64,
    validation_skipped: u64,
}

impl RuleHits {
    /// Exact number of rows with the given outcome (or all rows when `None`).
    /// The retained `hits` list is capped, so the collected length cannot be
    /// used as the total.
    fn total_for(&self, outcome: Option<RowOutcome>) -> u64 {
        match outcome {
            None => {
                self.passed + self.failed + self.skipped + self.validation_skipped
            }
            Some(RowOutcome::Passed) => self.passed,
            Some(RowOutcome::Failed) => self.failed,
            Some(RowOutcome::Skipped) => self.skipped,
            Some(RowOutcome::ValidationSkipped) => self.validation_skipped,
        }
    }
}

/// The row opened in the floating detail form.
#[derive(Debug, Clone)]
struct DetailState {
    /// Title shown in the form header.
    title: String,
    /// `(attribute, value)` pairs, in column order.
    fields: Vec<(String, String)>,
    /// Rule the row came from, when opened from the rules panel. Enables the
    /// "rule attributes only" mode.
    rule: Option<usize>,
}

/// A prefix index for one column: one `(lowercased value, row byte offset)`
/// entry per data row, sorted by value. A `beginsWith` search is then a binary
/// search followed by a forward scan while the prefix still matches.
#[derive(Debug, Clone)]
struct ColumnIndex {
    entries: Vec<(String, u64)>,
}

impl ColumnIndex {
    /// Byte offsets of the rows whose value starts with `prefix`, in file order.
    /// Matching is case-insensitive (keys are stored lowercased).
    fn prefix_offsets(&self, prefix: &str) -> Vec<u64> {
        let prefix = prefix.to_lowercase();
        let start = self
            .entries
            .partition_point(|(value, _)| value.as_str() < prefix.as_str());
        let mut offsets = Vec::new();
        for (value, offset) in &self.entries[start..] {
            if !value.starts_with(&prefix) {
                break;
            }
            offsets.push(*offset);
        }
        // Keys are sorted by value, not by position, so restore file order.
        offsets.sort_unstable();
        offsets
    }
}

/// A named set of visible attributes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProfileConfig {
    #[serde(default)]
    visible: Vec<String>,
}

/// The whole persisted configuration file (`<config>/fview/profiles.toml`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Config {
    #[serde(default)]
    profiles: BTreeMap<String, ProfileConfig>,
}

/// Resolve the path of the TOML profile store.
fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("fview").join("profiles.toml"))
}

/// Load the profile store, falling back to an empty set on any I/O or parse
/// error (a broken config should never stop the viewer from opening).
fn load_config() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

/// Persist the profile store as pretty TOML, creating the directory if needed.
fn store_config(config: &Config) -> Result<(), String> {
    let Some(path) = config_path() else {
        return Err("cannot determine a config directory".into());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let text =
        toml::to_string_pretty(config).map_err(|e| format!("cannot serialize profiles: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Case-insensitive substring test used by the attribute filter. An empty
/// filter never matches (used for highlighting) and never hides (used for the
/// hidden list), so callers handle the empty case explicitly where needed.
fn attr_matches(filter: &str, name: &str) -> bool {
    let filter = filter.trim();
    !filter.is_empty() && name.to_lowercase().contains(&filter.to_lowercase())
}

/// Note shown when the hidden-attribute list is collapsed, so the count stays
/// visible without spending vertical space on the chips.
fn hidden_note(count: usize) -> String {
    match count {
        1 => "1 hidden attribute available — use the Hidden button to reveal it".to_string(),
        _ => format!(
            "{count} hidden attributes available — use the Hidden button to reveal them"
        ),
    }
}

/// Status line for the current scan. `rows_read` is the number of data rows
/// read; when the scan was not truncated it is the total number of rows in the
/// file. `elapsed` appends how long the search itself took.
/// The `(left, right, expected)` condition a hit was grouped under in the
/// distinct-row mode.
fn condition_key(hit: &RowHit) -> (String, String, Option<String>) {
    (hit.left.clone(), hit.right.clone(), hit.expected.clone())
}

/// Heading shown above one distinct-condition group, naming the wrong value the
/// rows share.
fn condition_heading(hit: &RowHit) -> String {
    match &hit.expected {
        Some(expected) => format!("expected “{expected}” · saw “{}”", hit.right),
        None => format!("“{}” vs “{}”", hit.left, hit.right),
    }
}

fn status_text(
    shown: usize,
    matched: usize,
    rows_read: usize,
    truncated: bool,
    indexed: bool,
    elapsed: Option<Duration>,
) -> String {
    let summary = if indexed {
        if truncated {
            format!("showing first {shown} of {matched} matching rows (index prefix)")
        } else {
            format!("{matched} matching rows (index prefix)")
        }
    } else if truncated {
        if matched > shown {
            format!("showing first {shown} of {matched} matching rows · {rows_read} rows read")
        } else {
            format!("showing first {shown} matching rows (more available) · {rows_read} rows read")
        }
    } else if matched == rows_read {
        format!("{rows_read} rows")
    } else {
        format!("{matched} matching rows of {rows_read} total")
    };

    match elapsed {
        Some(elapsed) => format!("{summary} · {}", format_duration(elapsed)),
        None => summary,
    }
}

/// Compact duration for the status line: milliseconds below one second, seconds
/// with two decimals above.
fn format_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 1.0 {
        format!("{} ms", elapsed.as_millis())
    } else {
        format!("{seconds:.2} s")
    }
}

/// Width of the docked rules sidebar for a given window, kept in sync with the
/// container built by `rules_sidebar`.
fn sidebar_width(window_width: f32) -> f32 {
    (window_width * 0.30).clamp(260.0, 340.0)
}

/// Width available to one line of chips: the grid minus the row list's right
/// padding and the stripe's horizontal padding.
fn chip_area_width(grid_width: f32) -> f32 {
    (grid_width - LIST_RIGHT_PADDING - 2.0 * STRIPE_PADDING_H).max(200.0)
}

/// Estimated width of the chip holding `value`, labelled with `header` when the
/// attribute names are shown. Mirrors `estimate_chip_width` without building
/// the `"header = value"` string on every row of the height pass.
fn chip_estimate(header: &str, value: &str, show_name: bool) -> f32 {
    let chars = if show_name {
        header.chars().count() + 3 + value.chars().count()
    } else {
        value.chars().count()
    };
    chars as f32 * CHAR_WIDTH + CHIP_CHROME
}

/// Greedy first-fit: how many lines are needed to lay `widths` out within
/// `available`, keeping `CHIP_SPACING` between neighbouring chips. A chip wider
/// than the whole line counts as one line. Mirrored by `chip_lines`, which
/// returns the actual ranges.
fn chip_line_count(widths: impl Iterator<Item = f32>, available: f32) -> usize {
    let mut lines = 1usize;
    let mut used = 0.0f32;
    let mut count = 0usize;
    for width in widths {
        let width = width.min(available);
        if count > 0 && used + CHIP_SPACING + width > available {
            lines += 1;
            used = width;
        } else {
            used += if count > 0 { CHIP_SPACING } else { 0.0 } + width;
        }
        count += 1;
    }
    lines
}

/// Greedy first-fit line ranges for `widths` within `available`. Uses the same
/// packing as `chip_line_count` so the virtual-scroll heights match exactly
/// what is rendered.
fn chip_lines(widths: &[f32], available: f32) -> Vec<std::ops::Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut used = 0.0f32;
    for (index, &width) in widths.iter().enumerate() {
        let width = width.min(available);
        if index > start && used + CHIP_SPACING + width > available {
            lines.push(start..index);
            start = index;
            used = width;
        } else {
            used += if index > start { CHIP_SPACING } else { 0.0 } + width;
        }
    }
    if start < widths.len() {
        lines.push(start..widths.len());
    }
    lines
}

/// Height reserved for one line of chips, tall enough for the maximum number
/// of wrapped lines a chip may show.
fn chip_line_box() -> f32 {
    MAX_CHIP_LINES as f32 * CHIP_LINE_HEIGHT + CHIP_CHROME_V
}

/// Fixed height of a data stripe showing `lines` lines of chips. Used both to
/// place rows and to give each stripe exactly that height, keeping virtual
/// scrolling stable.
fn stripe_height(lines: usize) -> f32 {
    let lines = lines.max(1);
    lines as f32 * chip_line_box()
        + (lines - 1) as f32 * CHIP_LINE_SPACING
        + 2.0 * STRIPE_PADDING
}

/// Filter box the user last typed in, so Escape clears the expected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterFocus {
    Results,
    Attributes,
}

#[derive(Debug, Clone)]
enum Message {
    OpenFile,
    FileChosen(Option<PathBuf>),
    FilterChanged(String),
    RunFilter,
    /// Fired by the debounce timer; starts a scan once typing has paused.
    DebounceTick,
    /// Search only the attributes that are currently visible.
    ToggleVisibleOnly(bool),
    /// Use the parallel, full-file scan instead of the sequential early-exit one.
    ToggleParallel(bool),
    /// Render the matches as a table of the visible attributes instead of chips.
    ToggleTable(bool),
    /// Use the built column indexes (prefix search) instead of the regex.
    ToggleUseIndex(bool),
    /// Maximum number of matching rows to display; changing it re-runs the scan.
    LimitSelected(usize),
    /// Build or drop the prefix index for an attribute.
    ToggleIndex(usize),
    /// Open the detail form for a matching row (index into `rows`).
    RowClicked(usize),
    /// A chip was clicked (row, column): copies its value and opens the detail
    /// form when the click is part of a double click.
    ChipClicked(usize, usize),
    /// Show the attribute name in chip labels instead of the value alone.
    ToggleAttributeNames(bool),
    /// Close the row detail form.
    CloseDetail,
    /// Escape: close the row form, or clear the filter box being edited.
    Escape,
    /// Copy an attribute value from the detail form: `(attribute, value)`.
    CopyValue(String, String),
    /// Hide an attribute (column index) from the rows.
    Mute(usize),
    /// Show a previously hidden attribute again.
    Unmute(usize),
    UnmuteAll,
    /// Hide every attribute at once, so a few can be picked back.
    MuteAll,
    /// Pin/unpin an attribute so it is always visible in the results.
    ToggleLock(usize),
    /// Expand or collapse the list of hidden attribute chips.
    ToggleHidden,
    /// The attribute search box changed: highlight matching chips and narrow
    /// the hidden attribute list.
    AttributeFilterChanged(String),
    /// A saved profile was picked from the dropdown.
    ProfileSelected(String),
    /// Clear the selected profile (attributes stay as they are).
    ClearProfile,
    /// Overwrite the selected profile with the current visible attributes.
    SaveCurrentProfile,
    /// Open the "save as new profile" name prompt.
    BeginSaveNewProfile,
    NewProfileNameChanged(String),
    ConfirmSaveNewProfile,
    CancelSaveNewProfile,
    /// `(generation, result)`; stale generations are ignored.
    ScanFinished(u64, Result<ScanResult, String>),
    /// Show or hide the rule-evaluation panel.
    ToggleRulesPanel,
    /// Show or hide the configuration panel.
    ToggleConfigPanel,
    /// Open a native picker for the rules DSL file.
    OpenRules,
    RulesChosen(Option<PathBuf>),
    /// `(generation, result)`; stale generations are ignored.
    RulesEvaluated(u64, Result<EvaluatedRules, String>),
    /// Collect the complete row list for one rule (click on a rule name).
    RuleAllRows(usize),
    /// Show only one outcome of a rule's rows: `(rule, outcome)`.
    RuleFilterRows(usize, RowOutcome),
    /// `(generation, rule, result)`; stale generations are ignored.
    RuleRowsCollected(u64, usize, Result<RuleHits, String>),
    /// Stop filtering the main grid and return to the regular scan results.
    ClearRuleView,
    /// Show only the attributes referenced by the row's rule.
    ToggleRuleAttrsOnly(bool),
    /// Sample a rule's collected rows per distinct failing condition.
    ToggleDistinct(bool),
    /// `(column, path, result)`; a background column-index build finished. The
    /// path is carried so a build that outlives a file switch is discarded.
    IndexBuilt(usize, PathBuf, Result<ColumnIndex, String>),
    /// The window was resized; used to wrap chips and to size the virtual list.
    Resized(f32, f32),
    /// The scroll position changed; drives the virtual row window.
    Scrolled(scrollable::Viewport),
}

struct Viewer {
    path: Option<PathBuf>,
    delimiter: u8,
    case_sensitive: bool,
    limit: usize,
    headers: Vec<String>,
    muted: HashSet<usize>,
    /// Columns the user pinned with the lock icon. A locked column is never
    /// hidden: `mute`, `mute all`, profiles and "rule attributes only" all keep
    /// it visible.
    locked: HashSet<usize>,
    /// Whether the list of hidden attribute chips in the top bar is expanded.
    show_hidden: bool,
    filter: String,
    /// The rows currently shown. Rows are shared (`Arc`) so a rule-view filter
    /// switch re-uses the cached records instead of deep-copying them.
    rows: Vec<Arc<Vec<String>>>,
    /// Per-row group heading, aligned with `rows`. `Some` when the row starts a
    /// new distinct-condition group (rule view, distinct mode); the grid renders
    /// the heading above that row.
    row_groups: Vec<Option<String>>,
    /// The row opened in the floating detail form.
    detail: Option<DetailState>,
    /// Attribute whose value was last copied from the detail form, so the form
    /// can confirm the copy.
    copy_notice: Option<String>,
    /// Whether the rule-evaluation panel is open.
    show_rules: bool,
    /// Whether the configuration panel is open.
    show_config: bool,
    /// Rule set + evaluation state for the panel.
    rules: RulesState,
    /// Render `attribute = value` chip labels; off by default so a chip shows
    /// just the value.
    show_attr_names: bool,
    /// Filter box the user last typed in, so Escape clears that one first.
    filter_focus: Option<FilterFocus>,
    /// Last chip click `(when, row, column)`, used to detect a double click.
    last_chip_click: Option<(Instant, usize, usize)>,
    truncated: bool,
    /// Best known total number of matching rows from the last scan.
    matched: usize,
    /// Number of data rows read by the last completed scan.
    rows_read: usize,
    error: Option<String>,
    scanning: bool,
    /// When the in-flight scan started, used to time the search.
    scan_started: Option<Instant>,
    /// Duration of the last completed scan, shown in the status line.
    scan_duration: Option<Duration>,
    dirty: bool,
    /// Filter text used when the last scan was started, so a redundant rescan
    /// of an unchanged pattern is skipped.
    last_scanned: Option<String>,
    /// Whether a filter edit is waiting out the debounce quiet period.
    debounce_pending: bool,
    /// Time of the last filter keystroke, used by the debounce timer.
    last_edit: Option<Instant>,
    /// Search only the currently visible attributes (skips hidden ones).
    visible_only: bool,
    /// Use the parallel full-file scan instead of the sequential early-exit one.
    parallel: bool,
    /// Render the matches as a table of the visible attributes instead of chips.
    table: bool,
    /// Use the built column indexes (prefix search) instead of the regex.
    use_index: bool,
    /// Built prefix indexes, keyed by column index.
    indexes: HashMap<usize, Arc<ColumnIndex>>,
    /// Whether a column-index build is currently running.
    indexing: bool,
    /// Feedback about index actions, shown in the controls bar.
    index_status: Option<String>,
    /// Whether the last completed scan used an index (drives the status line).
    indexed_result: bool,
    generation: u64,
    window_width: f32,
    /// Stable id of the row scrollable, so the view can jump back to the top.
    scroll_id: scrollable::Id,
    /// Vertical scroll offset of the row list, in px.
    scroll_offset: f32,
    /// Height of the row viewport, in px.
    viewport_height: f32,
    /// Attribute search: highlights matching chips in the main view and narrows
    /// the hidden attribute list to the matching names.
    attribute_filter: String,
    /// Saved profiles, keyed by name.
    profiles: BTreeMap<String, ProfileConfig>,
    /// Profile currently applied, if any.
    current_profile: Option<String>,
    /// Whether the "save as new profile" name prompt is open.
    naming_profile: bool,
    new_profile_name: String,
    /// Short feedback message about profile actions (shown next to the controls).
    profile_status: Option<String>,
}

impl Viewer {
    fn new(args: Args) -> (Self, Task<Message>) {
        let delimiter = parse_delimiter(&args.delimiter).unwrap_or(b',');

        let mut viewer = Viewer {
            path: None,
            delimiter,
            case_sensitive: args.case_sensitive,
            limit: args.limit.max(1),
            headers: Vec::new(),
            muted: HashSet::new(),
            locked: HashSet::new(),
            show_hidden: false,
            filter: String::new(),
            rows: Vec::new(),
            row_groups: Vec::new(),
            detail: None,
            copy_notice: None,
            show_rules: false,
            show_config: true,
            rules: RulesState::default(),
            show_attr_names: false,
            filter_focus: None,
            last_chip_click: None,
            truncated: false,
            matched: 0,
            rows_read: 0,
            error: None,
            scanning: false,
            scan_started: None,
            scan_duration: None,
            dirty: false,
            last_scanned: None,
            debounce_pending: false,
            last_edit: None,
            visible_only: false,
            parallel: false,
            table: false,
            use_index: true,
            indexes: HashMap::new(),
            indexing: false,
            index_status: None,
            indexed_result: false,
            generation: 0,
            window_width: 1200.0,
            scroll_id: scrollable::Id::unique(),
            scroll_offset: 0.0,
            viewport_height: 720.0,
            attribute_filter: String::new(),
            profiles: load_config().profiles,
            current_profile: None,
            naming_profile: false,
            new_profile_name: String::new(),
            profile_status: None,
        };

        let task = match args.path {
            Some(path) => viewer.load_file(path),
            None => Task::none(),
        };

        (viewer, task)
    }

    /// Open a native file picker on a background task.
    fn pick_file() -> Task<Message> {
        Task::perform(
            async {
                rfd::AsyncFileDialog::new()
                    .add_filter("CSV / TSV", &["csv", "tsv", "txt"])
                    .pick_file()
                    .await
                    .map(|handle| handle.path().to_path_buf())
            },
            Message::FileChosen,
        )
    }

    /// Switch to a new file: reset the per-file state, read its headers and
    /// start scanning. Any in-flight scan is invalidated by the generation bump.
    fn load_file(&mut self, path: PathBuf) -> Task<Message> {
        self.generation += 1;
        self.scanning = false;
        self.scan_started = None;
        self.scan_duration = None;
        self.dirty = false;
        self.rows.clear();
        self.row_groups.clear();
        self.detail = None;
        self.copy_notice = None;
        // The evaluated report belonged to the previous file; drop it (but keep
        // the chosen rules file and id column for a quick re-evaluation).
        self.rules.generation += 1;
        self.rules.report = None;
        self.rules.plan = None;
        discard_rule_hits(self.rules.hits.take());
        self.rules.hits_filter = None;
        self.rules.view_active = false;
        self.rules.pending_rule = None;
        self.rules.evaluating = false;
        self.rules.collecting = false;
        self.rules.queued_rule = None;
        self.rules.saved_muted = None;
        self.rules.error = None;
        self.last_chip_click = None;
        self.filter_focus = None;
        self.truncated = false;
        self.matched = 0;
        self.rows_read = 0;
        self.last_scanned = None;
        self.debounce_pending = false;
        self.last_edit = None;
        self.indexes.clear();
        self.indexing = false;
        self.index_status = None;
        self.indexed_result = false;
        self.muted.clear();
        self.locked.clear();
        self.show_hidden = false;
        self.error = None;
        self.current_profile = None;
        self.naming_profile = false;
        self.new_profile_name.clear();
        self.profile_status = None;
        self.path = Some(path.clone());

        match read_headers(&path, self.delimiter) {
            Ok(headers) => {
                self.headers = headers;
                self.start_scan()
            }
            Err(message) => {
                self.headers.clear();
                self.error = Some(message);
                Task::none()
            }
        }
    }

    /// Names of the attributes currently visible (the active set).
    fn visible_names(&self) -> Vec<String> {
        self.headers
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.muted.contains(index))
            .map(|(_, name)| name.clone())
            .collect()
    }

    /// Apply a saved profile: attributes listed in it become visible, every
    /// other attribute is treated as hidden.
    fn apply_profile(&mut self, name: &str) {
        let Some(profile) = self.profiles.get(name).cloned() else {
            self.profile_status = Some(format!("unknown profile “{name}”"));
            return;
        };
        let visible: HashSet<&str> = profile.visible.iter().map(String::as_str).collect();
        self.muted = self
            .headers
            .iter()
            .enumerate()
            .filter(|(index, header)| {
                !visible.contains(header.as_str()) && !self.locked.contains(index)
            })
            .map(|(index, _)| index)
            .collect();
        self.current_profile = Some(name.to_string());
        self.profile_status = Some(format!("applied “{name}”"));
    }

    /// Persist the current profile set to the TOML store.
    fn persist(&self) -> Result<(), String> {
        store_config(&Config {
            profiles: self.profiles.clone(),
        })
    }

    /// Overwrite the profile that is currently selected with the attributes
    /// that are visible right now.
    fn save_current_profile(&mut self) {
        let Some(name) = self.current_profile.clone() else {
            return;
        };
        let visible = self.visible_names();
        self.profiles.insert(name.clone(), ProfileConfig { visible });
        match self.persist() {
            Ok(()) => self.profile_status = Some(format!("saved “{name}”")),
            Err(message) => self.profile_status = Some(message),
        }
    }

    /// Save the visible attributes under the name typed in the prompt.
    fn save_new_profile(&mut self) {
        let name = self.new_profile_name.trim().to_string();
        if name.is_empty() {
            self.profile_status = Some("enter a profile name".into());
            return;
        }
        let visible = self.visible_names();
        self.profiles.insert(name.clone(), ProfileConfig { visible });
        match self.persist() {
            Ok(()) => {
                self.current_profile = Some(name.clone());
                self.naming_profile = false;
                self.new_profile_name.clear();
                self.profile_status = Some(format!("saved “{name}”"));
            }
            Err(message) => self.profile_status = Some(message),
        }
    }

    /// Kick off a background scan. If one is already running we just mark the
    /// state dirty, which coalesces bursts of typing into a single re-scan.
    fn start_scan(&mut self) -> Task<Message> {
        let Some(path) = self.path.clone() else {
            return Task::none();
        };
        // A fresh search always leaves the rule-filtered grid and abandons any
        // rule-row collection (the result would otherwise overwrite the scan).
        // The collected rows are dropped too, so a large rule list does not
        // linger in memory after the user leaves the rule view.
        self.rules.view_active = false;
        discard_rule_hits(self.rules.hits.take());
        self.abort_rule_collection();
        self.sync_rule_attrs();
        if self.scanning {
            self.dirty = true;
            return Task::none();
        }

        self.error = None;
        self.scanning = true;
        self.scan_started = Some(Instant::now());
        self.dirty = false;
        self.generation += 1;
        let generation = self.generation;
        let delimiter = self.delimiter;
        let pattern = self.filter.clone();
        let case_sensitive = self.case_sensitive;
        let limit = self.limit;
        let parallel = self.parallel;
        // When enabled, only the columns that are currently visible are
        // searched; hidden attributes are skipped entirely.
        let visible = if self.visible_only {
            Some(
                (0..self.headers.len())
                    .map(|index| !self.muted.contains(&index))
                    .collect::<Vec<bool>>(),
            )
        } else {
            None
        };
        // A built index turns the search into a case-insensitive `beginsWith`
        // prefix query over the indexed columns. `--case-sensitive` and an empty
        // pattern keep the regex path.
        let indexes: Vec<Arc<ColumnIndex>> = self
            .indexes
            .iter()
            .filter(|(column, _)| !self.visible_only || !self.muted.contains(column))
            .map(|(_, index)| Arc::clone(index))
            .collect();
        let index_mode = self.index_mode() && !self.filter.trim().is_empty();
        self.last_scanned = Some(pattern.clone());

        if index_mode {
            Task::perform(
                async move { scan_indexed(path, delimiter, pattern, indexes, limit) },
                move |result| Message::ScanFinished(generation, result),
            )
        } else {
            Task::perform(
                async move {
                    scan(
                        path,
                        delimiter,
                        pattern,
                        case_sensitive,
                        limit,
                        visible,
                        parallel,
                    )
                },
                move |result| Message::ScanFinished(generation, result),
            )
        }
    }

    /// Kick off a background evaluation of the selected rules file against the
    /// open CSV. The report (with its sample ids) fills the rules panel.
    fn start_rules_evaluation(&mut self) -> Task<Message> {
        let Some(csv) = self.path.clone() else {
            self.rules.error = Some("open a CSV file first".into());
            return Task::none();
        };
        let Some(rules_path) = self.rules.path.clone() else {
            self.rules.error = Some("select a rules file first".into());
            return Task::none();
        };
        let restore_grid = self.rules.view_active;
        self.rules.evaluating = true;
        self.rules.error = None;
        discard_rule_hits(self.rules.hits.take());
        self.rules.hits_filter = None;
        self.rules.view_active = false;
        self.rules.plan = None;
        self.rules.pending_rule = None;
        self.rules.collecting = false;
        self.rules.queued_rule = None;
        self.sync_rule_attrs();
        self.rules.generation += 1;
        let generation = self.rules.generation;
        let delimiter = self.delimiter;
        let headers = self.headers.clone();
        let eval = Task::perform(
            async move {
                // Compile once; the plan is returned so single-rule collections
                // never reload the file or rebuild the plan.
                let mut program = dsl::load_file(&rules_path)?;
                program.defaults.report_limit = 50;
                let plan = rules::compile(program, &headers)?;
                let report = run_plan(&plan, &csv, delimiter, None, usize::MAX, false)?;
                Ok(EvaluatedRules {
                    plan: Arc::new(plan),
                    report,
                })
            },
            move |result| Message::RulesEvaluated(generation, result),
        );
        if restore_grid {
            Task::batch([eval, self.start_scan()])
        } else {
            eval
        }
    }

    /// The rule whose rows currently fill the main grid, if any.
    fn active_rule(&self) -> Option<usize> {
        if self.rules.view_active {
            self.rules.hits.as_ref().map(|hits| hits.rule)
        } else {
            None
        }
    }

    /// Redirect the main grid to a rule's rows. `filter` selects the outcome to
    /// show. Clicking the same view again clears it and returns the grid to the
    /// regular scan results.
    fn show_rule_rows(&mut self, rule: usize, filter: Option<RowOutcome>) -> Task<Message> {
        // Toggling the exact view off restores the scan results.
        if self.rules.view_active
            && self.rules.hits.as_ref().map(|hits| hits.rule) == Some(rule)
            && self.rules.hits_filter == filter
        {
            return self.clear_rule_view();
        }
        // Showing rule rows takes over the grid, so abandon any running scan.
        self.cancel_scan();
        // Data already collected for this rule in the current mode: just switch
        // side / re-activate. A changed distinct mode invalidates the sample,
        // so it falls through to a fresh collection.
        let fresh = self
            .rules
            .hits
            .as_ref()
            .is_some_and(|hits| hits.rule == rule && hits.distinct == self.rules.distinct);
        if fresh {
            self.abort_rule_collection();
            self.rules.hits_filter = filter;
            self.rules.view_active = true;
            return self.apply_rule_view();
        }
        // A collection for this rule is already running: keep it and just let
        // the grid show the outcome the user picked last when it lands.
        if self.rules.collecting && self.rules.pending_rule == Some(rule) {
            self.rules.queued_rule = None;
            self.rules.hits_filter = filter;
            return Task::none();
        }
        // Another rule is still being collected: queue this request instead of
        // running a second whole-file pass in parallel.
        if self.rules.collecting {
            self.rules.queued_rule = Some((rule, filter));
            return Task::none();
        }
        let (Some(csv), Some(plan)) = (self.path.clone(), self.rules.plan.clone()) else {
            self.rules.error = Some("evaluate the rules before browsing their rows".into());
            return Task::none();
        };
        // Collect the rule's rows and their full CSV records in one background
        // pass that evaluates *only* this rule, then let the response fill the
        // grid.
        self.rules.collecting = true;
        self.rules.pending_rule = Some(rule);
        discard_rule_hits(self.rules.hits.take());
        self.rules.hits_filter = filter;
        self.rules.error = None;
        self.rules.collect_started = Some(Instant::now());
        self.rules.generation += 1;
        let generation = self.rules.generation;
        let delimiter = self.delimiter;
        let limit = self.limit;
        let distinct = self.rules.distinct;
        Task::perform(
            async move { collect_rule_hits(plan, rule, csv, delimiter, limit, distinct) },
            move |result| Message::RuleRowsCollected(generation, rule, result),
        )
    }

    /// Drop any in-flight or queued rule-row collection. The generation bump
    /// makes a late result from the abandoned pass harmless, so it cannot
    /// overwrite the view the user just picked.
    fn abort_rule_collection(&mut self) {
        if self.rules.collecting || self.rules.pending_rule.is_some() {
            self.rules.generation += 1;
        }
        self.rules.collecting = false;
        self.rules.pending_rule = None;
        self.rules.queued_rule = None;
        self.rules.collect_started = None;
    }

    /// Abandon an in-flight scan so its result cannot overwrite the grid once
    /// rule rows are shown.
    fn cancel_scan(&mut self) {
        self.generation += 1;
        self.scanning = false;
        self.scan_started = None;
        self.dirty = false;
    }

    /// Stop showing rule rows in the grid and re-run the normal scan.
    fn clear_rule_view(&mut self) -> Task<Message> {
        self.rules.view_active = false;
        self.abort_rule_collection();
        self.sync_rule_attrs();
        self.start_scan()
    }

    /// Apply (or lift) the "rule attributes only" restriction over the grid's
    /// hidden-attribute set. While the mode is on and a rule is shown, every
    /// column the rule does not read is hidden; the user's previous set is
    /// restored when the mode or the rule view ends.
    fn sync_rule_attrs(&mut self) {
        if self.rules.attrs_only {
            if let Some(rule) = self.active_rule() {
                if self.rules.saved_muted.is_none() {
                    self.rules.saved_muted = Some(self.muted.clone());
                }
                let keep: HashSet<usize> = self.rule_columns(rule).iter().copied().collect();
                self.muted = (0..self.headers.len())
                    .filter(|column| !keep.contains(column) && !self.locked.contains(column))
                    .collect();
                return;
            }
        }
        if let Some(saved) = self.rules.saved_muted.take() {
            // A column locked while the mode was on must stay visible.
            self.muted = saved
                .into_iter()
                .filter(|column| !self.locked.contains(column))
                .collect();
        }
    }

    /// Fill the main grid with the currently selected side of the collected
    /// rule rows.
    fn apply_rule_view(&mut self) -> Task<Message> {
        let (mut rows, mut groups, matching, total) = {
            let Some(hits) = &self.rules.hits else {
                return Task::none();
            };
            let filter = self.rules.hits_filter;
            // Indices of the hits to show, in file order.
            let mut selected: Vec<usize> = hits
                .hits
                .iter()
                .enumerate()
                .filter(|(_, hit)| filter.is_none() || filter == Some(hit.outcome))
                .map(|(index, _)| index)
                .collect();
            let mut groups: Vec<Option<String>> = Vec::with_capacity(selected.len());
            if hits.distinct {
                // Distinct mode groups equal conditions together and labels the
                // first row of each group, so it is clear which wrong value a
                // block of rows belongs to.
                selected.sort_by(|&a, &b| {
                    condition_key(&hits.hits[a]).cmp(&condition_key(&hits.hits[b]))
                });
                let mut last: Option<(String, String, Option<String>)> = None;
                for &index in &selected {
                    let hit = &hits.hits[index];
                    let key = condition_key(hit);
                    if last.as_ref() != Some(&key) {
                        groups.push(Some(condition_heading(hit)));
                        last = Some(key);
                    } else {
                        groups.push(None);
                    }
                }
            } else {
                groups.resize(selected.len(), None);
            }
            let rows: Vec<Arc<Vec<String>>> = selected
                .iter()
                .map(|&index| Arc::clone(&hits.rows[index]))
                .collect();
            (
                rows,
                groups,
                hits.total_for(filter) as usize,
                hits.total_for(None) as usize,
            )
        };
        // Honor the "rows" drop-down even in the rule view: the filter stays in
        // force, only the number of displayed rows changes. `matched` keeps the
        // full count so the status line can say how many were truncated.
        rows.truncate(self.limit);
        groups.truncate(self.limit);
        self.rows = rows;
        self.row_groups = groups;
        self.matched = matching;
        self.truncated = matching > self.rows.len();
        self.rows_read = total;
        self.indexed_result = false;
        self.error = None;
        self.scroll_offset = 0.0;
        self.sync_rule_attrs();
        scrollable::scroll_to(self.scroll_id.clone(), AbsoluteOffset { x: 0.0, y: 0.0 })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenFile => Self::pick_file(),
            Message::FileChosen(Some(path)) => self.load_file(path),
            Message::FileChosen(None) => Task::none(),
            Message::FilterChanged(value) => {
                // Do not scan on every keystroke: record the edit and let the
                // debounce timer start a single scan once typing pauses.
                self.filter = value;
                self.filter_focus = Some(FilterFocus::Results);
                self.last_edit = Some(Instant::now());
                self.debounce_pending = true;
                Task::none()
            }
            Message::DebounceTick => {
                if !self.debounce_pending {
                    return Task::none();
                }
                let quiet = self
                    .last_edit
                    .map(|at| at.elapsed() >= Duration::from_millis(DEBOUNCE_QUIET_MS))
                    .unwrap_or(true);
                if !quiet {
                    return Task::none();
                }
                self.debounce_pending = false;
                // Skip the scan entirely when the pattern did not change.
                if self.last_scanned.as_deref() == Some(self.filter.as_str()) {
                    return Task::none();
                }
                self.start_scan()
            }
            Message::RunFilter => {
                self.debounce_pending = false;
                self.start_scan()
            }
            Message::ToggleVisibleOnly(checked) => {
                self.visible_only = checked;
                self.start_scan()
            }
            Message::ToggleParallel(checked) => {
                self.parallel = checked;
                self.start_scan()
            }
            Message::ToggleTable(checked) => {
                self.table = checked;
                Task::none()
            }
            Message::ToggleUseIndex(checked) => {
                self.use_index = checked;
                self.start_scan()
            }
            Message::LimitSelected(limit) => {
                self.limit = limit.max(1);
                // The rule view is a filter, not a scan: changing how many rows
                // are shown must keep the rule / outcome filter in place. The
                // collected list is capped per outcome, so a larger limit needs
                // a fresh collection for that one rule.
                if self.rules.view_active {
                    let recollect = self
                        .rules
                        .hits
                        .as_ref()
                        .is_some_and(|hits| hits.cap < self.limit);
                    if recollect {
                        if let Some(rule) = self.active_rule() {
                            let filter = self.rules.hits_filter;
                            discard_rule_hits(self.rules.hits.take());
                            self.rules.view_active = false;
                            return self.show_rule_rows(rule, filter);
                        }
                    }
                    return self.apply_rule_view();
                }
                self.start_scan()
            }
            Message::ToggleIndex(column) => {
                if self.indexes.remove(&column).is_some() {
                    self.index_status = Some(format!("dropped index on “{}”", self.header(column)));
                    // The search falls back to the regex now.
                    self.start_scan()
                } else if self.indexing {
                    self.index_status = Some("an index build is already running".into());
                    Task::none()
                } else if let Some(path) = self.path.clone() {
                    self.indexing = true;
                    self.index_status =
                        Some(format!("indexing “{}”…", self.header(column)));
                    let delimiter = self.delimiter;
                    let built_path = path.clone();
                    Task::perform(
                        async move { build_index(path, delimiter, column) },
                        move |result| Message::IndexBuilt(column, built_path.clone(), result),
                    )
                } else {
                    Task::none()
                }
            }
            Message::RowClicked(index) => {
                self.open_detail(index);
                Task::none()
            }
            Message::ChipClicked(row, column) => {
                let Some(value) = self.rows.get(row).and_then(|values| values.get(column)).cloned()
                else {
                    return Task::none();
                };
                // A second click on the same chip within the double-click window
                // also opens the row form, so a clipped value can be read in
                // full (the tooltip covers the quick look case).
                let now = Instant::now();
                let double = self
                    .last_chip_click
                    .map(|(when, row0, column0)| {
                        row0 == row
                            && column0 == column
                            && now.duration_since(when) < Duration::from_millis(400)
                    })
                    .unwrap_or(false);
                self.last_chip_click = if double {
                    None
                } else {
                    Some((now, row, column))
                };
                if double {
                    self.open_detail(row);
                }
                self.copy_notice = Some(self.header(column).to_string());
                iced::clipboard::write(value)
            }
            Message::ToggleAttributeNames(checked) => {
                self.show_attr_names = checked;
                Task::none()
            }
            Message::CloseDetail => {
                self.detail = None;
                self.copy_notice = None;
                Task::none()
            }
            Message::Escape => {
                // Escape closes the row form first. Otherwise it clears the
                // filter box the user was last editing, falling back to the
                // other one when that box is already empty; clearing the regex
                // re-runs the scan.
                if self.detail.is_some() {
                    self.detail = None;
                    self.copy_notice = None;
                    return Task::none();
                }
                let attributes_first = self.filter_focus == Some(FilterFocus::Attributes);
                if attributes_first && !self.attribute_filter.is_empty() {
                    self.attribute_filter.clear();
                } else if !attributes_first && !self.filter.is_empty() {
                    self.filter.clear();
                    self.debounce_pending = false;
                    return self.start_scan();
                } else if !self.attribute_filter.is_empty() {
                    self.attribute_filter.clear();
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.debounce_pending = false;
                    return self.start_scan();
                }
                Task::none()
            }
            Message::CopyValue(attribute, value) => {
                self.copy_notice = Some(attribute);
                iced::clipboard::write(value)
            }
            Message::Mute(index) => {
                // A pinned column is always visible, so hiding it is ignored.
                if self.locked.contains(&index) {
                    return Task::none();
                }
                self.muted.insert(index);
                self.profile_status = None;
                self.rescan_if_searching_visible()
            }
            Message::Unmute(index) => {
                self.muted.remove(&index);
                self.profile_status = None;
                self.rescan_if_searching_visible()
            }
            Message::UnmuteAll => {
                self.muted.clear();
                self.profile_status = None;
                self.rescan_if_searching_visible()
            }
            Message::MuteAll => {
                // Pinned columns stay visible even when everything else is
                // hidden.
                self.muted = (0..self.headers.len())
                    .filter(|column| !self.locked.contains(column))
                    .collect();
                self.profile_status = None;
                self.rescan_if_searching_visible()
            }
            Message::ToggleLock(index) => {
                if self.locked.remove(&index) {
                    // Unlocking keeps the column visible; the eye icon hides it.
                } else {
                    self.locked.insert(index);
                    self.muted.remove(&index);
                }
                self.profile_status = None;
                self.sync_rule_attrs();
                self.rescan_if_searching_visible()
            }
            Message::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                Task::none()
            }
            Message::AttributeFilterChanged(value) => {
                self.attribute_filter = value;
                self.filter_focus = Some(FilterFocus::Attributes);
                Task::none()
            }
            Message::ProfileSelected(name) => {
                self.apply_profile(&name);
                Task::none()
            }
            Message::ClearProfile => {
                self.current_profile = None;
                self.profile_status = None;
                Task::none()
            }
            Message::SaveCurrentProfile => {
                self.save_current_profile();
                Task::none()
            }
            Message::BeginSaveNewProfile => {
                self.naming_profile = true;
                self.new_profile_name.clear();
                self.profile_status = None;
                Task::none()
            }
            Message::NewProfileNameChanged(value) => {
                self.new_profile_name = value;
                Task::none()
            }
            Message::ConfirmSaveNewProfile => {
                self.save_new_profile();
                Task::none()
            }
            Message::CancelSaveNewProfile => {
                self.naming_profile = false;
                self.new_profile_name.clear();
                self.profile_status = None;
                Task::none()
            }
            Message::ToggleConfigPanel => {
                self.show_config = !self.show_config;
                Task::none()
            }
            Message::ToggleRulesPanel => {
                self.show_rules = !self.show_rules;
                Task::none()
            }
            Message::OpenRules => Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .add_filter("Rule DSL", &["vl", "rules", "txt"])
                        .pick_file()
                        .await
                        .map(|handle| handle.path().to_path_buf())
                },
                Message::RulesChosen,
            ),
            Message::RulesChosen(path) => {
                if let Some(path) = path {
                    self.rules.path = Some(path);
                    self.rules.report = None;
                    self.rules.plan = None;
                    discard_rule_hits(self.rules.hits.take());
                    self.rules.queued_rule = None;
                    self.rules.error = None;
                    self.start_rules_evaluation()
                } else {
                    Task::none()
                }
            }
            Message::RulesEvaluated(generation, result) => {
                if generation != self.rules.generation {
                    return Task::none();
                }
                self.rules.evaluating = false;
                self.rules.queued_rule = None;
                match result {
                    Ok(evaluated) => {
                        self.rules.plan = Some(evaluated.plan);
                        self.rules.report = Some(evaluated.report);
                        self.rules.error = None;
                        discard_rule_hits(self.rules.hits.take());
                        self.rules.hits_filter = None;
                        self.rules.view_active = false;
                        self.rules.pending_rule = None;
                    }
                    Err(message) => {
                        self.rules.plan = None;
                        self.rules.report = None;
                        self.rules.error = Some(message);
                    }
                }
                Task::none()
            }
            Message::RuleAllRows(rule) => self.show_rule_rows(rule, None),
            Message::RuleFilterRows(rule, outcome) => {
                self.show_rule_rows(rule, Some(outcome))
            }
            Message::RuleRowsCollected(generation, _rule, result) => {
                if generation != self.rules.generation {
                    return Task::none();
                }
                self.rules.collecting = false;
                self.rules.pending_rule = None;
                self.rules.collect_duration =
                    self.rules.collect_started.take().map(|start| start.elapsed());
                let task = match result {
                    Ok(hits) => {
                        // Only one rule's rows are held at a time; the next
                        // collection drops these before it runs.
                        self.rules.hits = Some(hits);
                        self.rules.view_active = true;
                        self.apply_rule_view()
                    }
                    Err(message) => {
                        discard_rule_hits(self.rules.hits.take());
                        self.rules.view_active = false;
                        self.rules.error = Some(message);
                        Task::none()
                    }
                };
                // A rule the user asked for while this pass was running is
                // evaluated now, so clicks are never dropped.
                if let Some((queued, filter)) = self.rules.queued_rule.take() {
                    Task::batch([task, self.show_rule_rows(queued, filter)])
                } else {
                    task
                }
            }
            Message::ClearRuleView => self.clear_rule_view(),
            Message::ToggleRuleAttrsOnly(checked) => {
                self.rules.attrs_only = checked;
                // Narrow (or restore) the grid's visible attributes right away.
                self.sync_rule_attrs();
                Task::none()
            }
            Message::ToggleDistinct(checked) => {
                self.rules.distinct = checked;
                // A fresh sample is needed; the collected rows were capped by
                // the previous mode.
                if let Some(rule) = self.active_rule() {
                    let filter = self.rules.hits_filter;
                    discard_rule_hits(self.rules.hits.take());
                    self.rules.view_active = false;
                    return self.show_rule_rows(rule, filter);
                }
                Task::none()
            }
            Message::Resized(width, height) => {
                // Keep the virtual viewport fresh so a taller window renders more
                // rows without waiting for the next scroll event.
                self.viewport_height = height.max(1.0);
                // The chip layout and the row form size themselves from the
                // window width, so always record the latest value.
                self.window_width = width;
                Task::none()
            }
            Message::Scrolled(viewport) => {
                self.scroll_offset = viewport.absolute_offset().y;
                self.viewport_height = viewport.bounds().height.max(1.0);
                Task::none()
            }
            Message::ScanFinished(generation, result) => {
                if generation != self.generation {
                    return Task::none();
                }
                match result {
                    Ok(scan) => {
                        self.rows = scan.rows.into_iter().map(Arc::new).collect();
                        self.row_groups.clear();
                        self.matched = scan.matched;
                        self.truncated = scan.truncated;
                        self.rows_read = scan.rows_read;
                        self.indexed_result = scan.indexed;
                        self.error = None;
                    }
                    Err(message) => self.error = Some(message),
                }
                self.scanning = false;
                self.scan_duration = self.scan_started.take().map(|start| start.elapsed());
                self.scroll_offset = 0.0;
                // Jump the row list back to the top for the new result set.
                let reset = scrollable::scroll_to(
                    self.scroll_id.clone(),
                    AbsoluteOffset { x: 0.0, y: 0.0 },
                );
                // Re-scan when a mute change or a filter edit landed while the
                // scan was running.
                let stale = self.last_scanned.as_deref() != Some(self.filter.as_str());
                if self.dirty || stale {
                    self.dirty = false;
                    Task::batch([reset, self.start_scan()])
                } else {
                    reset
                }
            }
            Message::IndexBuilt(column, path, result) => {
                // The file may have been switched while the index was building;
                // the offsets would be meaningless, so drop the result.
                if self.path.as_deref() != Some(path.as_path()) {
                    return Task::none();
                }
                self.indexing = false;
                match result {
                    Ok(index) => {
                        let count = index.entries.len();
                        let name = self.header(column).to_string();
                        self.indexes.insert(column, Arc::new(index));
                        self.index_status =
                            Some(format!("indexed “{name}” ({count} rows)"));
                        // Re-run the search: the index now serves a prefix query.
                        self.start_scan()
                    }
                    Err(message) => {
                        self.index_status = Some(message);
                        Task::none()
                    }
                }
            }
        }
    }

    /// The header of a column, or `?` when the index is out of range.
    fn header(&self, column: usize) -> &str {
        self.headers.get(column).map(String::as_str).unwrap_or("?")
    }

    /// Whether the search is currently served by the built indexes (a
    /// `beginsWith` prefix query) rather than the regex: at least one searched
    /// column is indexed, index search is enabled, and case-sensitive mode has
    /// not forced the regex path.
    fn index_mode(&self) -> bool {
        self.use_index
            && !self.case_sensitive
            && self
                .indexes
                .keys()
                .any(|column| !self.visible_only || !self.muted.contains(column))
    }

    /// Muting an attribute changes which columns are searched when the
    /// visible-only mode is on, so that mode needs the results recomputed.
    fn rescan_if_searching_visible(&mut self) -> Task<Message> {
        if self.visible_only {
            self.start_scan()
        } else {
            Task::none()
        }
    }

    /// Snapshot one matching row into the detail form, so the form keeps
    /// showing it even when a later scan replaces the visible matches.
    fn open_detail(&mut self, row: usize) {
        let fields = self.rows.get(row).map(|values| {
            (0..values.len())
                .map(|column| (self.header(column).to_string(), values[column].clone()))
                .collect()
        });
        if let Some(fields) = fields {
            // When the grid is showing a rule's rows, tag the form so the
            // "rule attributes only" mode can filter it.
            let rule = self.active_rule();
            let title = match rule {
                Some(rule) => format!("Rule {} · match {}", rule + 1, row + 1),
                None => format!("Match {}", row + 1),
            };
            self.detail = Some(DetailState {
                title,
                fields,
                rule,
            });
            self.copy_notice = None;
        }
    }

    /// Column indices referenced by a rule in the current report.
    fn rule_columns(&self, rule: usize) -> &[usize] {
        self.rules
            .report
            .as_ref()
            .and_then(|report| report.rules.get(rule))
            .map(|rule| rule.rule_columns.as_slice())
            .unwrap_or(&[])
    }

    /// App theme, handed to iced once at startup; every custom style above
    /// reads its colors from the palette it defines.
    fn theme(&self) -> Theme {
        modern_theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        let resize = iced::window::resize_events()
            .map(|(_id, size)| Message::Resized(size.width, size.height));
        // Poll only while an edit is pending; the handler waits for the quiet
        // period before starting the scan, which debounces typing bursts.
        let debounce = if self.debounce_pending {
            iced::time::every(Duration::from_millis(DEBOUNCE_TICK_MS))
                .map(|_| Message::DebounceTick)
        } else {
            Subscription::none()
        };
        // Escape closes the row form or clears the filter box being edited.
        // `listen_with` rather than `on_key_press`: a focused text input
        // captures Escape before a subscription that only sees ignored events
        // would receive it.
        let escape = if self.detail.is_some()
            || !self.filter.is_empty()
            || !self.attribute_filter.is_empty()
        {
            iced::event::listen_with(|event, _status, _window| match event {
                iced::event::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: Key::Named(keyboard::key::Named::Escape),
                    ..
                }) => Some(Message::Escape),
                _ => None,
            })
        } else {
            Subscription::none()
        };
        Subscription::batch([resize, debounce, escape])
    }

    fn view(&self) -> Element<'_, Message> {
        let theme = self.theme();
        let palette = theme.extended_palette();

        // No file yet: a single floating card with the Open call to action.
        if self.path.is_none() {
            return container(
                container(
                    column![
                        container(
                            text(char::from(Bootstrap::FileEarmarkSpreadsheetFill))
                                .font(BOOTSTRAP_FONT)
                                .size(30)
                                .color(palette.primary.base.color),
                        )
                        .padding(16)
                        .style(|theme: &Theme| container::Style {
                            background: Some(Background::Color(
                                theme.extended_palette().primary.weak.color,
                            )),
                            border: Border {
                                radius: 10.0.into(),
                                ..Border::default()
                            },
                            ..container::Style::default()
                        }),
                        text("No CSV file opened").size(24),
                        text("Grep and browse rows of a large CSV file.")
                            .size(14)
                            .color(muted_text(&theme)),
                        button(
                            row![
                                text(char::from(Bootstrap::FolderFill))
                                    .font(BOOTSTRAP_FONT)
                                    .size(16),
                                text("Open CSV…").size(16),
                            ]
                            .spacing(8)
                            .align_y(Center),
                        )
                        .on_press(Message::OpenFile)
                        .padding([12, 24])
                        .style(primary_button),
                    ]
                    .spacing(14)
                    .align_x(Center),
                )
                .padding(40)
                .style(card_style),
            )
            .center_x(Fill)
            .center_y(Fill)
            .into();
        }

        // Small app mark: an accent tile with the spreadsheet glyph.
        let brand = row![
            container(
                text(char::from(Bootstrap::FileEarmarkSpreadsheetFill))
                    .font(BOOTSTRAP_FONT)
                    .size(14)
                    .color(Color::WHITE),
            )
            .padding([5, 6])
            .style(|theme: &Theme| container::Style {
                background: Some(Background::Color(theme.extended_palette().primary.base.color)),
                border: Border {
                    radius: 4.0.into(),
                    ..Border::default()
                },
                ..container::Style::default()
            }),
            text("fview").size(16),
        ]
        .spacing(8)
        .align_y(Center);


        let file_name = self
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();

        let filter_hint = if self.index_mode() {
            "beginsWith prefix over indexed columns, e.g. Fra"
        } else {
            "regex filter, e.g. \\d{4}-\\d{2}"
        };
        let filter = text_input(filter_hint, &self.filter)
            .on_input(Message::FilterChanged)
            .on_submit(Message::RunFilter)
            .padding(10)
            .size(14)
            .width(Fill)
            .style(input_style);

        let search = button(
            row![
                text(char::from(Bootstrap::Search)).font(BOOTSTRAP_FONT),
                text("Search"),
            ]
            .spacing(6)
            .align_y(Center),
        )
        .on_press(Message::RunFilter)
        .padding([10, 18])
        .style(primary_button);

        let status: Element<'_, Message> = if let Some(error) = &self.error {
            row![
                text(char::from(Bootstrap::ExclamationTriangle))
                    .font(BOOTSTRAP_FONT)
                    .size(13)
                    .color(palette.danger.base.color),
                text(error.as_str()).size(13).color(palette.danger.base.color),
            ]
            .spacing(6)
            .align_y(Center)
            .into()
        } else if let Some(active) = self.active_rule() {
            let label = match self.rules.hits_filter {
                None => "all rows",
                Some(RowOutcome::Passed) => "matching rows",
                Some(RowOutcome::Failed) => "failing rows",
                Some(RowOutcome::Skipped) => "skipped rows",
                Some(RowOutcome::ValidationSkipped) => "validation-skipped rows",
            };
            // How long the rows took to extract, next to the rule view like the
            // duration on a regular search's status line.
            let elapsed = self
                .rules
                .collect_duration
                .map(|duration| format!(" · {}", format_duration(duration)))
                .unwrap_or_default();
            row![
                text(char::from(Bootstrap::ClipboardCheck))
                    .font(BOOTSTRAP_FONT)
                    .size(13)
                    .color(palette.primary.base.color),
                text(format!(
                    "rule {} · {label} ({}){elapsed}",
                    active + 1,
                    self.rows.len()
                ))
                .size(13)
                .color(palette.primary.base.color),
                button(text("clear").size(12))
                    .on_press(Message::ClearRuleView)
                    .padding([1, 8])
                    .style(ghost_button),
            ]
            .spacing(8)
            .align_y(Center)
            .into()
        } else if self.scanning {
            row![
                text(char::from(Bootstrap::HourglassSplit))
                    .font(BOOTSTRAP_FONT)
                    .size(13)
                    .color(palette.primary.base.color),
                text("scanning…").size(13).color(palette.primary.base.color),
            ]
            .spacing(6)
            .align_y(Center)
            .into()
        } else {
            // A rule view reports how long its rows took to collect, just like
            // a scan reports its own duration.
            let elapsed = if self.active_rule().is_some() {
                self.rules.collect_duration
            } else {
                self.scan_duration
            };
            text(status_text(
                self.rows.len(),
                self.matched,
                self.rows_read,
                self.truncated,
                self.indexed_result,
                elapsed,
            ))
            .size(13)
            .color(muted_text(&theme))
            .into()
        };

        // Opt-in scan modes: skip hidden columns, read the whole file in
        // parallel, render as a table, or use the built column indexes for a
        // `beginsWith` search. The "rows" drop-down sets how many matches are
        // kept.
        let mut limit_choices = ROW_LIMIT_CHOICES.to_vec();
        if !limit_choices.contains(&self.limit) {
            limit_choices.push(self.limit);
            limit_choices.sort_unstable();
        }
        let view_options = row![
            checkbox("visible only", self.visible_only)
                .on_toggle(Message::ToggleVisibleOnly)
                .text_size(12)
                .style(checkbox_style),
            checkbox("parallel", self.parallel)
                .on_toggle(Message::ToggleParallel)
                .text_size(12)
                .style(checkbox_style),
            checkbox("distinct", self.rules.distinct)
                .on_toggle(Message::ToggleDistinct)
                .text_size(12)
                .style(checkbox_style),
            checkbox("table", self.table)
                .on_toggle(Message::ToggleTable)
                .text_size(12)
                .style(checkbox_style),
            checkbox("index", self.use_index)
                .on_toggle_maybe((!self.indexes.is_empty()).then_some(Message::ToggleUseIndex))
                .text_size(12)
                .style(checkbox_style),
            checkbox("attribute names", self.show_attr_names)
                .on_toggle(Message::ToggleAttributeNames)
                .text_size(12)
                .style(checkbox_style),
        ]
        .spacing(12)
        .align_y(Center)
        .wrap();

        let rows_options = row![
            text("show").size(12).color(muted_text(&theme)),
            pick_list(limit_choices, Some(self.limit), Message::LimitSelected)
                .padding(4)
                .text_size(12)
                .style(pick_list_style),
            text("matching rows").size(12).color(muted_text(&theme)),
        ]
        .spacing(8)
        .align_y(Center)
        .wrap();


        let toolbar = container(
            row![
                text("Filter").size(13).color(muted_text(&theme)),
                filter,
                search,
            ]
            .spacing(10)
            .align_y(Center),
        )
        .padding([10, 12])
        .width(Fill)
        .style(card_style);
        // The status gets its own line so a long message cannot squeeze the
        // filter field in the bar above. It keeps a fixed height because the
        // scanning/error states are taller than plain status text and would
        // otherwise shift the table down by a few pixels. A chip or form copy
        // is confirmed on the right of the same line.
        let mut status_line = Row::new().spacing(8).align_y(Center).push(status);
        status_line = status_line.push(Space::with_width(Fill));
        if let Some(attribute) = &self.copy_notice {
            status_line = status_line.push(
                row![
                    text(char::from(Bootstrap::CheckLg))
                        .font(BOOTSTRAP_FONT)
                        .size(12)
                        .color(palette.success.strong.color),
                    text(format!("copied {attribute}"))
                        .size(12)
                        .color(palette.success.strong.color),
                ]
                .spacing(4)
                .align_y(Center),
            );
        }
        let status_bar = container(status_line)
            .height(Length::Fixed(STATUS_HEIGHT))
            .align_y(Center)
            .padding([0.0, 4.0]);

        // Chip geometry: how many chips fit on a line is decided per row by
        // greedy packing, so a row of narrow chips fills the line instead of
        // leaving a gap after a fixed column count.
        let all_hidden = !self.headers.is_empty() && self.muted.len() >= self.headers.len();
        let visible_columns: Vec<usize> = (0..self.headers.len())
            .filter(|index| !self.muted.contains(index))
            .collect();
        // The grid shrinks when the rules sidebar is docked, so size the chips
        // from the width the grid actually gets rather than the window width.
        // Using the window width here is what let a full line of chips overflow
        // (and get clipped) while the rules panel was open.
        // Every docked side panel takes its width from the grid before the
        // chips are packed, so a full line never overflows under a panel.
        let panel_width = sidebar_width(self.window_width);
        let mut grid_width = self.window_width - 2.0 * CONTENT_PADDING;
        if self.show_rules {
            grid_width -= panel_width + MAIN_GAP;
        }
        if self.show_config {
            grid_width -= panel_width + MAIN_GAP;
        }
        // Width a chip line may use, and how many hidden-attribute chips fit on
        // one line in the controls bar.
        let available = chip_area_width(grid_width - 2.0 * GRID_PADDING);
        // Hidden-attribute chips flow in a wrapping row inside the
        // configuration panel, so they pack the panel width instead of one
        // chip per line.

        // The controls are grouped under four headings — View, Rows, Attributes
        // and Profile — separated by hairlines so each section reads on its own.
        let mut hidden_bar = column![
            text("View").size(13).color(muted_text(&theme)),
            view_options,
            horizontal_rule(1).style(divider_style),
            text("Rows").size(13).color(muted_text(&theme)),
            rows_options,
            horizontal_rule(1).style(divider_style),
            text("Attributes").size(13).color(muted_text(&theme)),
        ]
        .spacing(8)
        .padding(0);
        let mut controls = Row::new().spacing(10).align_y(Center).width(Fill);
        controls = controls.push(
            button(
                row![
                    text(char::from(Bootstrap::EyeSlash))
                        .font(BOOTSTRAP_FONT)
                        .size(13),
                    text("Hide all").size(13),
                ]
                .spacing(5)
                .align_y(Center),
            )
            .on_press(Message::MuteAll)
            .padding([3, 8])
            .style(secondary_button),
        );
        if !self.muted.is_empty() {
            // Collapsible list of hidden attributes: long lists would otherwise
            // push the data rows off screen.
            let caret = if self.show_hidden {
                Bootstrap::CaretDown
            } else {
                Bootstrap::CaretRight
            };
            controls = controls.push(
                button(
                    row![
                        text(char::from(caret)).font(BOOTSTRAP_FONT).size(13),
                        text(format!("Hidden ({})", self.muted.len())).size(13),
                    ]
                    .spacing(5)
                    .align_y(Center),
                )
                .on_press(Message::ToggleHidden)
                .padding([3, 8])
                .style(secondary_button),
            );
            controls = controls.push(
                button(text("show all").size(13))
                    .on_press(Message::UnmuteAll)
                    .padding([3, 8])
                    .style(ghost_button),
            );
        }
        // The hide/unhide actions form one row under the "Attributes" heading;
        // the filter and the hidden-attribute list follow below it.
        hidden_bar = hidden_bar.push(controls.wrap());
        // Attribute filter: highlights matching chips in the main view and
        // narrows the hidden attribute list below to the matching names.
        hidden_bar = hidden_bar.push(
            text_input("filter attributes…", &self.attribute_filter)
                .on_input(Message::AttributeFilterChanged)
                .padding(8)
                .size(13)
                .style(input_style)
                .width(Length::Fill),
        );
        // The hidden attribute names are sorted alphabetically so a large
        // attribute list stays easy to scan. The list is collapsed by default
        // (a long list would otherwise push the rows off screen); it opens when
        // the user toggles it or searches for an attribute. When collapsed only
        // a note with the count is shown.
        if !self.muted.is_empty() {
            let searching = !self.attribute_filter.trim().is_empty();
            if !self.show_hidden {
                hidden_bar = hidden_bar.push(text(hidden_note(self.muted.len())).size(13));
            } else {
                let mut indices: Vec<usize> = self.muted.iter().copied().collect();
                indices.sort_by(|a, b| {
                    let left = self.headers.get(*a).map(String::as_str).unwrap_or("");
                    let right = self.headers.get(*b).map(String::as_str).unwrap_or("");
                    left.to_lowercase()
                        .cmp(&right.to_lowercase())
                        .then_with(|| a.cmp(b))
                });
                let mut line = Row::new().spacing(6).align_y(Center);
                let mut shown = 0usize;
                for index in indices {
                    let Some(name) = self.headers.get(index) else {
                        continue;
                    };
                    if searching && !attr_matches(&self.attribute_filter, name) {
                        continue;
                    }
                    // Mirror the visible chips: a database icon toggles the
                    // index and the eye reveals the attribute again.
                    let indexed = self.indexes.contains_key(&index);
                    let database = if indexed {
                        Bootstrap::DatabaseFill
                    } else {
                        Bootstrap::Database
                    };
                    line = line.push(
                        container(
                            row![
                                text(name).size(13),
                                button(
                                    text(char::from(database))
                                        .font(BOOTSTRAP_FONT)
                                        .size(14),
                                )
                                .on_press(Message::ToggleIndex(index))
                                .padding(2)
                                .style(ghost_button),
                                button(
                                    text(char::from(Bootstrap::Eye))
                                        .font(BOOTSTRAP_FONT)
                                        .size(14),
                                )
                                .on_press(Message::Unmute(index))
                                .padding(2)
                                .style(ghost_button),
                            ]
                            .spacing(6)
                            .align_y(Center),
                        )
                        .padding([3, 8])
                        .style(move |theme| chip_style(theme, false, indexed)),
                    );
                    shown += 1;
                }
                if shown > 0 {
                    hidden_bar = hidden_bar.push(line.wrap());
                } else {
                    hidden_bar =
                        hidden_bar.push(text("no hidden attributes match the filter").size(13));
                }
            }
        }

        // Profile controls sit on the right of the bar: pick a saved profile,
        // overwrite it, or save the current visible set under a new name.
        let profile_names: Vec<String> = self.profiles.keys().cloned().collect();
        let mut profile_controls = Row::new().spacing(10).align_y(Center).width(Fill);
        profile_controls = profile_controls.push(
            pick_list(
                profile_names,
                self.current_profile.clone(),
                Message::ProfileSelected,
            )
            .placeholder("none")
            .padding(8)
            .text_size(13)
            .style(pick_list_style),
        );
        if self.current_profile.is_some() {
            profile_controls = profile_controls.push(
                button(text("Save").size(13))
                    .on_press(Message::SaveCurrentProfile)
                    .padding([3, 8])
                    .style(primary_button),
            );
            profile_controls = profile_controls.push(
                button(text("clear").size(13))
                    .on_press(Message::ClearProfile)
                    .padding([3, 8])
                    .style(ghost_button),
            );
        }
        profile_controls = profile_controls.push(
            button(text("Save as new…").size(13))
                .on_press(Message::BeginSaveNewProfile)
                .padding([3, 8])
                .style(secondary_button),
        );
        if let Some(status) = &self.profile_status {
            profile_controls = profile_controls.push(text(status.as_str()).size(12));
        }
        if let Some(status) = &self.index_status {
            profile_controls = profile_controls.push(text(status.as_str()).size(12));
        }
        hidden_bar = hidden_bar.push(horizontal_rule(1).style(divider_style));
        hidden_bar = hidden_bar.push(text("Profile").size(13).color(muted_text(&theme)));
        hidden_bar = hidden_bar.push(profile_controls.wrap());

        // Prompt for the name of a new profile.
        if self.naming_profile {
            hidden_bar = hidden_bar.push(
                row![
                    text("New profile name:").size(13).color(muted_text(&theme)),
                    text_input("profile name", &self.new_profile_name)
                        .on_input(Message::NewProfileNameChanged)
                        .on_submit(Message::ConfirmSaveNewProfile)
                        .padding(8)
                        .size(13)
                        .style(input_style)
                        .width(Length::Fixed(200.0)),
                    button(text("Save").size(13))
                        .on_press(Message::ConfirmSaveNewProfile)
                        .padding([3, 8])
                        .style(primary_button),
                    button(text("Cancel").size(13))
                        .on_press(Message::CancelSaveNewProfile)
                        .padding([3, 8])
                        .style(ghost_button),
                ]
                .spacing(6)
                .align_y(Center),
            );
        }


        // The configuration has its own docked panel: the scan/view options,
        // the attribute controls, the profiles and the hidden attribute chips.
        let config_header = row![
            text(char::from(Bootstrap::Sliders))
                .font(BOOTSTRAP_FONT)
                .size(15)
                .color(palette.primary.base.color),
            text("Configuration").size(16),
            Space::with_width(Fill),
            button(text(char::from(Bootstrap::XLg)).font(BOOTSTRAP_FONT).size(14))
                .on_press(Message::ToggleConfigPanel)
                .padding([4, 8])
                .style(ghost_button),
        ]
        .spacing(8)
        .align_y(Center);

        let config_panel: Element<'_, Message> = container(
            column![
                config_header,
                scrollable(hidden_bar)
                    .height(Fill)
                    .width(Fill)
                    .style(scrollbar_style),
            ]
            .spacing(10)
            .height(Fill),
        )
        .width(Length::Fixed(panel_width))
        .height(Fill)
        .padding(12)
        .style(sidebar_style)
        .into();

        // Table geometry: each column keeps at least `TABLE_CELL_MIN_WIDTH`, so
        // the table grows horizontally instead of squeezing columns into the
        // window. `show_table` is false when there is nothing to tabulate.
        let show_table = self.table && !all_hidden && !self.headers.is_empty();
        let column_widths: Vec<f32> = visible_columns
            .iter()
            .map(|&column| {
                (self.header(column).chars().count() as f32 * CHAR_WIDTH
                    + 24.0
                    + TABLE_HEADER_CHROME)
                    .max(TABLE_CELL_MIN_WIDTH)
            })
            .collect();
        let table_width: f32 = column_widths.iter().sum::<f32>().max(1.0);

        // Right padding keeps the chips (and the table) clear of the scrollbar,
        // which iced draws over the right edge of the scrollable.
        let mut list = column![]
            .spacing(0)
            .padding(Padding {
                right: 14.0,
                ..Padding::ZERO
            });
        if self.rows.is_empty() {
            let message = if self.scanning {
                "scanning…"
            } else {
                "no rows match the filter"
            };
            list = list.push(container(text(message).size(14)).padding(12));
        } else if all_hidden {
            list = list.push(
                container(
                    text("All attributes hidden — reveal the Hidden list above, then click an attribute to display it.")
                        .size(14),
                )
                .padding(12),
            );
        } else {
            // Virtual scrolling: only the rows intersecting the viewport (plus a
            // small overscan) are built, so a long list costs the same per frame
            // as a short one. Chips are packed per row, so rows can have a
            // different number of lines; a running top offset per row lets the
            // scroll position be mapped back to a row index exactly.
            let total_rows = self.rows.len();
            let viewport = self.viewport_height.max(1.0);

            let mut row_heights: Vec<f32> = if show_table {
                vec![TABLE_ROW_HEIGHT; total_rows]
            } else {
                let show_names = self.show_attr_names;
                self.rows
                    .iter()
                    .map(|values| {
                        let lines = chip_line_count(
                            visible_columns.iter().map(|&column| {
                                chip_estimate(
                                    self.header(column),
                                    values.get(column).map(String::as_str).unwrap_or(""),
                                    show_names,
                                )
                            }),
                            available,
                        );
                        stripe_height(lines)
                    })
                    .collect()
            };
            // Distinct groups reserve extra vertical space for their heading.
            for (index, height) in row_heights.iter_mut().enumerate() {
                if self.row_groups.get(index).is_some_and(|group| group.is_some()) {
                    *height += GROUP_HEADING_HEIGHT;
                }
            }
            let mut row_tops = Vec::with_capacity(total_rows + 1);
            row_tops.push(0.0f32);
            for height in &row_heights {
                let last = row_tops[row_tops.len() - 1];
                row_tops.push(last + height);
            }
            let total_height = row_tops[total_rows];

            let first = row_tops
                .partition_point(|&top| top <= self.scroll_offset)
                .saturating_sub(1 + OVERSCAN_ROWS)
                .min(total_rows);
            let last = (row_tops
                .partition_point(|&top| top < self.scroll_offset + viewport)
                + OVERSCAN_ROWS
                + 1)
                .min(total_rows)
                .max(first);

            // The header lives inside the scrolled content so it stays aligned
            // when the table scrolls horizontally.
            if show_table {
                let mut header_line = Row::new().spacing(0);
                for (column_position, &column) in visible_columns.iter().enumerate() {
                    // The header itself is inert now: the database, mute and
                    // lock icons mirror the controls on the chip view.
                    let indexed = self.indexes.contains_key(&column);
                    let locked = self.locked.contains(&column);
                    let database = if indexed {
                        Bootstrap::DatabaseFill
                    } else {
                        Bootstrap::Database
                    };
                    let lock_icon = if locked {
                        Bootstrap::LockFill
                    } else {
                        Bootstrap::Unlock
                    };
                    let mut cell = row![
                        container(
                            text(self.header(column))
                                .size(14)
                                .wrapping(Wrapping::None),
                        )
                        .width(Fill)
                        .clip(true),
                        button(text(char::from(database)).font(BOOTSTRAP_FONT).size(14))
                            .on_press(Message::ToggleIndex(column))
                            .padding(2)
                            .style(ghost_button),
                    ]
                    .spacing(4)
                    .align_y(Center);
                    if !locked {
                        cell = cell.push(
                            button(
                                text(char::from(Bootstrap::EyeSlash))
                                    .font(BOOTSTRAP_FONT)
                                    .size(14),
                            )
                            .on_press(Message::Mute(column))
                            .padding(2)
                            .style(ghost_button),
                        );
                    }
                    cell = cell.push(
                        button(text(char::from(lock_icon)).font(BOOTSTRAP_FONT).size(14))
                            .on_press(Message::ToggleLock(column))
                            .padding(2)
                            .style(ghost_button),
                    );
                    header_line = header_line.push(
                        container(cell)
                            .width(Length::Fixed(column_widths[column_position]))
                            .padding([5, 10])
                            .style(move |theme: &Theme| header_cell_style(theme, indexed)),
                    );
                }
                list = list.push(
                    container(header_line)
                        .width(Length::Fixed(table_width))
                        .style(stripe_style(true)),
                );
                // Hairline under the header so the first row reads as data.
                list = list.push(
                    container(horizontal_rule(1).style(divider_style))
                        .width(Length::Fixed(table_width)),
                );
            }

            if first > 0 {
                list = list.push(Space::with_height(Length::Fixed(row_tops[first])));
            }

            for (offset, values) in self.rows[first..last].iter().enumerate() {
                let index = first + offset;
                let row_height = row_heights[index];
                let heading = self.row_groups.get(index).and_then(Option::as_ref);
                let data_height = row_height
                    - if heading.is_some() {
                        GROUP_HEADING_HEIGHT
                    } else {
                        0.0
                    };
                let mut stripe = column![].spacing(0);
                if let Some(heading) = heading {
                    stripe = stripe.push(
                        container(
                            text(heading.clone())
                                .size(12)
                                .color(muted_text(&theme)),
                        )
                        .width(if show_table {
                            Length::Fixed(table_width)
                        } else {
                            Length::Fill
                        })
                        .height(Length::Fixed(GROUP_HEADING_HEIGHT))
                        .padding([4, 10])
                        .style(stripe_style(true)),
                    );
                }
                if show_table {
                    let mut line = Row::new().spacing(0);
                    for (column_position, &column) in visible_columns.iter().enumerate() {
                        let cell = values.get(column).map(String::as_str).unwrap_or("");
                        line = line.push(
                            container(text(cell).size(14).wrapping(Wrapping::None))
                                .width(Length::Fixed(column_widths[column_position]))
                                .clip(true)
                                .padding([3, 10]),
                        );
                    }
                    let striped = index % 2 == 1;
                    stripe = stripe.push(
                        button(
                            container(line)
                                .width(Length::Fixed(table_width))
                                .height(Length::Fixed(data_height))
                                .clip(true),
                        )
                        .on_press(Message::RowClicked(index))
                        .padding(0)
                        .width(Length::Fixed(table_width))
                        .height(Length::Fixed(data_height))
                        .style(move |theme, status| row_button_style(theme, status, striped, true)),
                    );
                    list = list.push(stripe);
                    continue;
                }
                // Greedily pack the chips for this row: a chip that does not fit
                // in the remaining width starts the next line, so a row of
                // narrow chips uses the whole line instead of a fixed column
                // count.
                let show_names = self.show_attr_names;
                let widths: Vec<f32> = visible_columns
                    .iter()
                    .map(|&column| {
                        chip_estimate(
                            self.header(column),
                            values.get(column).map(String::as_str).unwrap_or(""),
                            show_names,
                        )
                    })
                    .collect();
                let mut chips = column![].spacing(CHIP_LINE_SPACING);
                for range in chip_lines(&widths, available) {
                    let mut line = Row::new().spacing(CHIP_SPACING);
                    for position in range {
                        let column = visible_columns[position];
                        let header = self.header(column);
                        let highlight = attr_matches(&self.attribute_filter, header);
                        let indexed = self.indexes.contains_key(&column);
                        let locked = self.locked.contains(&column);
                        line = line.push(chip(
                            header,
                            &values[column],
                            index,
                            column,
                            available,
                            highlight,
                            indexed,
                            locked,
                            show_names,
                        ));
                    }
                    // Clip each chip line to a fixed height so a very long value
                    // cannot make one line taller than the rest.
                    chips = chips.push(
                        container(line)
                            .height(Length::Fixed(chip_line_box()))
                            .clip(true),
                    );
                }
                // The stripe is a button too: clicking the row (but not a chip,
                // which captures its own click) opens the same attribute form as
                // a table row.
                let striped = index % 2 == 1;
                stripe = stripe.push(
                    button(
                        container(chips)
                            .width(Fill)
                            .height(Length::Fixed(data_height))
                            .clip(true)
                            .padding([STRIPE_PADDING, STRIPE_PADDING_H]),
                    )
                    .on_press(Message::RowClicked(index))
                    .padding(0)
                    .width(Fill)
                    .height(Length::Fixed(data_height))
                    .style(move |theme, status| row_button_style(theme, status, striped, false)),
                );
                list = list.push(stripe);
            }

            if last < total_rows {
                list = list.push(Space::with_height(Length::Fixed(
                    total_height - row_tops[last],
                )));
            }
        }

        // Table mode scrolls both ways: the columns keep a comfortable width and
        // the table grows horizontally rather than being squeezed into the
        // window.
        let direction = if show_table {
            scrollable::Direction::Both {
                vertical: scrollable::Scrollbar::default(),
                horizontal: scrollable::Scrollbar::default(),
            }
        } else {
            scrollable::Direction::Vertical(scrollable::Scrollbar::default())
        };

        let main_col: Element<'_, Message> = column![
            toolbar,
            status_bar,
            container(
                scrollable(list)
                    .id(self.scroll_id.clone())
                    .on_scroll(Message::Scrolled)
                    .direction(direction)
                    .style(scrollbar_style)
                    .height(Fill)
                    .width(Fill),
            )
            .width(Fill)
            .height(Fill)
            .clip(true)
            .padding(GRID_PADDING)
            .style(card_style),
        ]
        .spacing(8)
        .height(Fill)
        .into();

        // The rule panel docks on the left and the configuration panel on the
        // right; the grid takes whatever width is left.
        let mut body_row = Row::new()
            .spacing(MAIN_GAP)
            .width(Fill)
            .height(Fill);
        if self.show_rules {
            body_row = body_row.push(self.rules_sidebar());
        }
        body_row = body_row.push(main_col);
        if self.show_config {
            body_row = body_row.push(config_panel);
        }
        let content: Element<'_, Message> = body_row.into();
        // The top bar spans the full window width; the body below keeps the
        // canvas padding. The menu tabs mirror the toolbar's former buttons.
        let tabs = row![
            menu_tab(
                Some(char::from(Bootstrap::FolderFill)),
                "Open",
                false,
                Some(Message::OpenFile),
                &theme
            ),
            menu_tab(
                None,
                "Rules",
                self.show_rules,
                Some(Message::ToggleRulesPanel),
                &theme
            ),
            menu_tab(
                None,
                "Table",
                self.table,
                Some(Message::ToggleTable(!self.table)),
                &theme
            ),
            menu_tab(
                None,
                "Index",
                self.use_index && !self.indexes.is_empty(),
                (!self.indexes.is_empty()).then_some(Message::ToggleUseIndex(!self.use_index)),
                &theme,
            ),
        ]
        .spacing(2)
        .align_y(Center);

        // Opens the configuration panel; filled while the panel is open.
        let config_active = self.show_config;
        let config_toggle = button(
            text(char::from(Bootstrap::Sliders))
                .font(BOOTSTRAP_FONT)
                .size(14),
        )
        .on_press(Message::ToggleConfigPanel)
        .padding([7, 10])
        .style(move |theme: &Theme, status: button::Status| {
            if config_active {
                primary_button(theme, status)
            } else {
                secondary_button(theme, status)
            }
        });
        let app_bar = container(
            row![
                brand,
                separator(),
                tabs,
                Space::with_width(Fill),
                container(text(file_name).size(13).color(muted_text(&theme)))
                    .padding([4, 12])
                    .style(badge_style),
                config_toggle,
            ]
            .spacing(10)
            .align_y(Center),
        )
        .padding([6, 14])
        .width(Fill)
        .style(app_bar_style);

        let body = container(content)
            .padding(CONTENT_PADDING)
            .width(Fill)
            .height(Fill);

        let content: Element<'_, Message> = column![app_bar, body]
            .height(Fill)
            .into();

        // The row detail form floats above the table; `opaque` keeps clicks on
        // the backdrop from reaching the rows underneath. When it was opened
        // from the rules panel and the "rule attributes only" mode is on, only
        // the columns the rule references are shown.
        match &self.detail {
            Some(detail) => {
                let fields: Vec<(String, String)> = if self.rules.attrs_only {
                    detail
                        .rule
                        .map(|rule| {
                            let allowed = self.rule_columns(rule);
                            detail
                                .fields
                                .iter()
                                .enumerate()
                                .filter(|(index, _)| allowed.contains(index))
                                .map(|(_, pair)| pair.clone())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_else(|| detail.fields.clone())
                } else {
                    detail.fields.clone()
                };
                stack([
                    content,
                    detail_form(
                        &detail.title,
                        &fields,
                        self.copy_notice.as_deref(),
                        self.window_width,
                        &theme,
                    ),
                ])
                .into()
            }
            None => content,
        }
    }

    /// The rule-evaluation panel: pick/relaunch a rule file, then browse the
    /// per-rule rows (all outcomes). Clicking a rule loads its complete
    /// row list; clicking an outcome count filters it.
    fn rules_sidebar(&self) -> Element<'_, Message> {
        let theme = self.theme();
        let palette = theme.extended_palette();

        let rules_name = self
            .rules
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "no rules file".to_string());

        let open_rules = button(
            row![
                text(char::from(Bootstrap::FolderFill))
                    .font(BOOTSTRAP_FONT)
                    .size(13),
                text("Open rules…"),
            ]
            .spacing(6)
            .align_y(Center),
        )
        .on_press(Message::OpenRules)
        .padding([6, 12])
        .style(secondary_button);

        let close = button(text(char::from(Bootstrap::XLg)).font(BOOTSTRAP_FONT).size(14))
            .on_press(Message::ToggleRulesPanel)
            .padding([4, 8])
            .style(ghost_button);

        let mut header = column![
            row![
                text(char::from(Bootstrap::ClipboardCheck))
                    .font(BOOTSTRAP_FONT)
                    .size(15)
                    .color(palette.primary.base.color),
                text("Rules").size(16),
                Space::with_width(Fill),
                close,
            ]
            .spacing(8)
            .align_y(Center),
            row![
                open_rules,
                container(
                    text(rules_name)
                        .size(12)
                        .color(muted_text(&theme))
                        .wrapping(Wrapping::Word),
                )
                .padding([3, 8])
                .width(Fill)
                .style(badge_style),
            ]
            .spacing(8)
            .align_y(Center),
            checkbox("rule attributes only", self.rules.attrs_only)
                .on_toggle(Message::ToggleRuleAttrsOnly)
                .text_size(12)
                .style(checkbox_style),
        ]
        .spacing(8);

        if self.rules.evaluating {
            header = header.push(
                row![
                    text(char::from(Bootstrap::HourglassSplit))
                        .font(BOOTSTRAP_FONT)
                        .size(13)
                        .color(palette.primary.base.color),
                    text("evaluating rules…")
                        .size(13)
                        .color(palette.primary.base.color),
                ]
                .spacing(6)
                .align_y(Center),
            );
        }

        if let Some(error) = &self.rules.error {
            header = header.push(
                row![
                    text(char::from(Bootstrap::ExclamationTriangle))
                        .font(BOOTSTRAP_FONT)
                        .size(13)
                        .color(palette.danger.base.color),
                    text(error.as_str()).size(13).color(palette.danger.base.color),
                ]
                .spacing(6)
                .align_y(Center),
            );
        }

        // Only the per-rule statistics live here; the rows are shown in the
        // main grid when a rule (or one of its sides) is selected.
        let mut stats = column![].spacing(6);
        if let Some(report) = &self.rules.report {
            stats = stats.push(
                text(format!(
                    "{} rules · {} passed · {} failed",
                    report.rules_total, report.rules_passed, report.rules_failed
                ))
                .size(12)
                .color(muted_text(&theme)),
            );
            for (index, rule) in report.rules.iter().enumerate() {
                stats = stats.push(self.rule_card(index, rule, &theme));
            }
        } else if !self.rules.evaluating && self.rules.error.is_none() {
            stats = stats.push(
                text("Open a rules file to evaluate it against the CSV.")
                    .size(12)
                    .color(muted_text(&theme)),
            );
        }

        let body = column![
            header,
            scrollable(stats)
                .height(Fill)
                .width(Fill)
                .style(scrollbar_style),
        ]
        .spacing(10)
        .height(Fill);

        // Keep the sidebar comfortable without starving the grid on a narrow
        // window.
        let sidebar_width = sidebar_width(self.window_width);
        container(body)
            .width(Length::Fixed(sidebar_width))
            .height(Fill)
            .padding(12)
            .style(sidebar_style)
            .into()
    }

    /// One rule inside the rules panel. The name loads all of the rule's rows
    /// into the main grid; the `passed` / `failed` counts load just that side.
    /// While the grid is showing this rule, the aggregated sample is replaced
    /// by a note so the panel stays a summary.
    fn rule_card<'a>(
        &'a self,
        index: usize,
        rule: &'a RuleReport,
        theme: &Theme,
    ) -> Element<'a, Message> {
        let status = if rule.passed() { "passed" } else { "failed" };
        let active = self.active_rule() == Some(index);
        // The rule being collected is highlighted too, so the panel shows which
        // rule the grid is (about to be) showing while the background pass runs.
        let collecting = self.rules.collecting && self.rules.pending_rule == Some(index);
        let selected = active || collecting;
        let filter = if selected { self.rules.hits_filter } else { None };
        let caret = if selected {
            Bootstrap::CaretDown
        } else {
            Bootstrap::CaretRight
        };

        let name_button = button(
            row![
                text(char::from(caret)).font(BOOTSTRAP_FONT).size(12),
                text(format!("{}. {}", index + 1, rule.name))
                    .size(14)
                    .width(Fill)
                    .wrapping(Wrapping::Word),
            ]
            .spacing(8)
            .align_y(Center),
        )
        .on_press(Message::RuleAllRows(index))
        .padding([4, 4])
        .width(Fill)
        .style(ghost_button);

        let passed_button = outcome_button(
            index,
            RowOutcome::Passed,
            format!("passed {}", rule.rows_passed),
            filter,
        );
        let failed_button = outcome_button(
            index,
            RowOutcome::Failed,
            format!("failed {}", rule.rows_failed),
            filter,
        );
        let skipped_button = outcome_button(
            index,
            RowOutcome::Skipped,
            format!("skipped {}", rule.rows_skipped),
            filter,
        );
        let validation_skipped_button = outcome_button(
            index,
            RowOutcome::ValidationSkipped,
            format!("validation skipped {}", rule.rows_validation_skipped),
            filter,
        );

        // Statistics go on their own line: a long rule name in the narrow
        // sidebar must never squeeze them into one letter per line.
        // The status pill rides on the title line, right-aligned, like the rule
        // cards in the reference; the outcome counts get their own line below.
        let title_row = row![
            name_button,
            container(text(status).size(11))
                .padding([2, 8])
                .style(move |theme: &Theme| status_badge_style(theme, status)),
        ]
        .spacing(6)
        .align_y(Center);

        // Statistics go on their own line: a long rule name in the narrow
        // sidebar must never squeeze them into one letter per line.
        let stats = row![
            text(format!("checked {}", rule.rows_checked))
                .size(12)
                .color(muted_text(theme)),
            passed_button,
            failed_button,
            skipped_button,
            validation_skipped_button,
        ]
        .spacing(6)
        .align_y(Center)
        .wrap();

        let mut card = column![title_row, stats].spacing(6).padding(8);

        if active {
            let label = match filter {
                None => "all rows",
                Some(RowOutcome::Passed) => "matching rows",
                Some(RowOutcome::Failed) => "failing rows",
                Some(RowOutcome::Skipped) => "skipped rows",
                Some(RowOutcome::ValidationSkipped) => "validation-skipped rows",
            };
            card = card.push(
                row![
                    text(char::from(Bootstrap::ClipboardCheck))
                        .font(BOOTSTRAP_FONT)
                        .size(12)
                        .color(theme.extended_palette().primary.base.color),
                    text(format!("{label} shown in the grid"))
                        .size(11)
                        .width(Fill)
                        .wrapping(Wrapping::Word)
                        .color(muted_text(theme)),
                    button(text("clear").size(11))
                        .on_press(Message::ClearRuleView)
                        .padding([1, 8])
                        .style(ghost_button),
                ]
                .spacing(6)
                .align_y(Center),
            );
        } else if collecting {
            card = card.push(
                row![
                    text(char::from(Bootstrap::HourglassSplit))
                        .font(BOOTSTRAP_FONT)
                        .size(12),
                    text("collecting rows…").size(12).color(muted_text(theme)),
                ]
                .spacing(6)
                .align_y(Center),
            );
        }

        container(card)
            .width(Fill)
            .style(move |theme| rule_card_style(theme, selected))
            .into()
    }
}

/// The app theme: a soft neutral canvas with a single indigo accent. Two
/// palettes are defined so a dark-mode desktop keeps a dark window.
fn modern_theme() -> Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(build_theme).clone()
}

/// Built once: OS dark mode is only read at startup, so the palette does not
/// have to be regenerated on every redraw.
fn build_theme() -> Theme {
    let palette = if matches!(Theme::default(), Theme::Dark) {
        Palette {
            background: Color::from_rgb(0.075, 0.086, 0.114),
            text: Color::from_rgb(0.902, 0.910, 0.937),
            primary: Color::from_rgb(0.376, 0.549, 0.980),
            success: Color::from_rgb(0.204, 0.780, 0.596),
            danger: Color::from_rgb(0.937, 0.353, 0.353),
        }
    } else {
        Palette {
            // Light app canvas (#F1F2F4) under white cards, like the reference.
            background: Color::from_rgb(0.945, 0.949, 0.957),
            text: Color::from_rgb(0.067, 0.094, 0.153),
            primary: Color::from_rgb(0.145, 0.388, 0.922),
            success: Color::from_rgb(0.086, 0.639, 0.290),
            danger: Color::from_rgb(0.863, 0.149, 0.149),
        }
    };
    Theme::custom("fview".to_string(), palette)
}

/// Secondary text: the theme text color faded so hierarchy stays readable in
/// both light and dark mode.
fn muted_text(theme: &Theme) -> Color {
    Color {
        a: 0.6,
        ..theme.extended_palette().background.base.text
    }
}

/// White surface used by the cards and the sidebar; slightly lifted in dark mode.
fn surface_color(theme: &Theme) -> Color {
    if theme.extended_palette().is_dark {
        Color::from_rgb(0.118, 0.129, 0.169)
    } else {
        Color::WHITE
    }
}

/// Hairline border shared by cards, inputs and chips (#E5E7EB in light mode).
fn hairline(theme: &Theme) -> Color {
    if theme.extended_palette().is_dark {
        Color::from_rgba(1.0, 1.0, 1.0, 0.10)
    } else {
        Color::from_rgb(0.898, 0.906, 0.922)
    }
}

/// Very light fill used for badges, table headers and hovered rows.
fn subtle_fill(theme: &Theme) -> Color {
    if theme.extended_palette().is_dark {
        Color::from_rgba(1.0, 1.0, 1.0, 0.04)
    } else {
        Color::from_rgb(0.973, 0.976, 0.980)
    }
}

/// A translucent tint of `color`. Selection states use this instead of an opaque
/// paint so the surface behind a card, pill or row shows through.
fn tint(color: Color, alpha: f32) -> Color {
    Color { a: alpha, ..color }
}

/// Floating panel: white surface, hairline border and a soft drop shadow.
fn card_style(theme: &Theme) -> container::Style {
    let dark = theme.extended_palette().is_dark;
    container::Style {
        background: Some(Background::Color(surface_color(theme))),
        border: Border {
            color: hairline(theme),
            width: 1.0,
            radius: CARD_RADIUS.into(),
        },
        shadow: Shadow {
            color: if dark {
                Color::from_rgba(0.0, 0.0, 0.0, 0.35)
            } else {
                Color::from_rgba(0.06, 0.09, 0.16, 0.04)
            },
            offset: Vector::new(0.0, 1.0),
            blur_radius: 8.0,
        },
        text_color: None,
    }
}

/// Pill used for the file name and other small metadata badges.
fn badge_style(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(subtle_fill(theme))),
        border: Border {
            color: hairline(theme),
            width: 1.0,
            radius: 999.0.into(),
        },
        ..container::Style::default()
    }
}

/// Rounded, softly tinted text input that highlights the accent while focused.
fn input_style(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let palette = theme.extended_palette();
    let focused = matches!(status, text_input::Status::Focused);
    let background = if palette.is_dark {
        Color::from_rgba(1.0, 1.0, 1.0, 0.05)
    } else {
        Color::WHITE
    };
    text_input::Style {
        background: Background::Color(background),
        border: Border {
            color: if focused {
                palette.primary.base.color
            } else {
                hairline(theme)
            },
            width: if focused { 1.5 } else { 1.0 },
            radius: RADIUS.into(),
        },
        icon: palette.background.base.text,
        placeholder: muted_text(theme),
        value: palette.background.base.text,
        selection: palette.primary.weak.color,
    }
}

/// Solid accent button (Open, Search, Save).
fn primary_button(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::primary(theme, status);
    // The contrast helper can pick dark text on the accent blue; the reference
    // style always uses white on the filled button.
    style.text_color = Color::WHITE;
    style.border.radius = RADIUS.into();
    style.shadow = Shadow {
        color: Color::from_rgba(0.06, 0.09, 0.16, 0.10),
        offset: Vector::new(0.0, 1.0),
        blur_radius: 3.0,
    };
    style
}

fn secondary_button(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let (background, text_color, border_color) = match status {
        button::Status::Hovered | button::Status::Pressed => (
            palette.primary.weak.color,
            palette.primary.strong.color,
            palette.primary.base.color,
        ),
        button::Status::Disabled => (subtle_fill(theme), muted_text(theme), hairline(theme)),
        button::Status::Active => (
            surface_color(theme),
            palette.background.base.text,
            hairline(theme),
        ),
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: Border {
            color: border_color,
            width: 1.0,
            radius: RADIUS.into(),
        },
        shadow: Shadow::default(),
    }
}

/// Borderless button used for inline/icon actions.
fn ghost_button(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::text(theme, status);
    style.border.radius = RADIUS.into();
    style
}

/// Rounded checkbox with the accent color and white tick.
fn checkbox_style(theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let checked = matches!(
        status,
        checkbox::Status::Active { is_checked: true }
            | checkbox::Status::Hovered { is_checked: true }
    );
    let mut style = if checked {
        checkbox::primary(theme, status)
    } else {
        checkbox::secondary(theme, status)
    };
    style.border.radius = 3.0.into();
    style.border.width = 1.0;
    style
}

/// Rounded drop-down matching the text inputs.
fn pick_list_style(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let palette = theme.extended_palette();
    let text = palette.background.base.text;
    let mut style = pick_list::default(theme, status);
    style.border.radius = RADIUS.into();
    // The built-in placeholder/handle colors are too faint on the card surface.
    style.text_color = text;
    style.placeholder_color = Color { a: 0.75, ..text };
    style.handle_color = Color { a: 0.7, ..text };
    style
}

/// Thin vertical rule used to group the controls bar into clusters.
fn separator() -> Element<'static, Message> {
    container(Space::new(
        Length::Fixed(1.0),
        Length::Fixed(18.0),
    ))
    .style(|theme: &Theme| container::Style {
        background: Some(Background::Color(
            theme.extended_palette().background.strong.color,
        )),
        ..container::Style::default()
    })
    .into()
}

/// Flat top bar: white surface with a hairline frame, no floating shadow.
fn app_bar_style(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_color(theme))),
        border: Border {
            color: hairline(theme),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// One tab in the top menu bar: a label with an accent underline when active.
fn menu_tab<'a>(
    icon: Option<char>,
    label: &'a str,
    active: bool,
    on_press: Option<Message>,
    theme: &Theme,
) -> Element<'a, Message> {
    let accent = theme.extended_palette().primary.base.color;
    let has_icon = icon.is_some();
    // Underline width is measured from the label (plus its icon) so the tabs
    // pack tight instead of a `Fill` bar stretching them across the whole bar.
    let width = (label.chars().count() as f32 * 6.4 + if has_icon { 20.0 } else { 0.0 })
        .max(18.0);
    let underline = container(Space::new(
        Length::Fixed(width),
        Length::Fixed(2.0),
    ))
    .style(move |_theme: &Theme| container::Style {
        background: active.then_some(Background::Color(accent)),
        border: Border {
            radius: 1.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    });
    let color = if active {
        theme.extended_palette().background.base.text
    } else {
        muted_text(theme)
    };
    let mut title = Row::new().spacing(6).align_y(Center);
    if let Some(icon) = icon {
        title = title.push(
            text(icon)
                .font(BOOTSTRAP_FONT)
                .size(13)
                .color(color),
        );
    }
    title = title.push(text(label).size(14).color(color));
    let content = column![title, underline].spacing(5).align_x(Center);
    // Center the *label* rather than the label-plus-underline block: the
    // underline (spacing + 2px bar) hangs below the midline, so the tab would
    // otherwise sit ~3.5px above the brand text. The extra top padding exactly
    // cancels that, without changing the tab's overall height.
    let mut tab = button(content)
        .padding(Padding {
            top: 10.5,
            right: 14.0,
            bottom: 3.5,
            left: 14.0,
        })
        .style(ghost_button);
    if let Some(message) = on_press {
        tab = tab.on_press(message);
    }
    tab.into()
}

fn header_cell_style(theme: &Theme, indexed: bool) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(if indexed {
            tint(
                palette.success.base.color,
                if palette.is_dark { 0.20 } else { 0.12 },
            )
        } else {
            subtle_fill(theme)
        })),
        ..container::Style::default()
    }
}

/// Row background for the table rows and the chip stripes: zebra striping, and
/// (for the table) an accent tint on hover since the whole row opens the form.
/// The chip stripes pass `hover_highlight = false`: the row is still clickable,
/// but hovering it must not flash a highlight behind the chips.
fn row_button_style(
    theme: &Theme,
    status: button::Status,
    striped: bool,
    hover_highlight: bool,
) -> button::Style {
    let palette = theme.extended_palette();
    let hovered = hover_highlight
        && matches!(status, button::Status::Hovered | button::Status::Pressed);
    let background = if hovered {
        Some(Background::Color(tint(
            palette.primary.base.color,
            if palette.is_dark { 0.18 } else { 0.10 },
        )))
    } else if striped {
        Some(Background::Color(subtle_fill(theme)))
    } else {
        None
    };
    button::Style {
        background,
        text_color: palette.background.base.text,
        border: Border::default(),
        shadow: Shadow::default(),
    }
}

/// Row height of one field in the detail form; also used to size the body.
const FORM_ROW_HEIGHT: f32 = 36.0;

/// Modal form with every attribute of one matching row. The panel is sized from
/// the field count so a short record does not leave a mostly empty box.
fn detail_form<'a>(
    title: &'a str,
    fields: &[(String, String)],
    copy_notice: Option<&str>,
    window_width: f32,
    theme: &Theme,
) -> Element<'a, Message> {
    let palette = theme.extended_palette();
    // Follow the window instead of a fixed width, so the form stays usable in a
    // narrow window and does not sprawl in a wide one.
    let panel_width = (window_width * 0.55).clamp(360.0, 820.0);
    let label_width = (panel_width * 0.28).clamp(90.0, 170.0);
    let mut form = column![]
        .spacing(6)
        // Clear of the scrollbar, which is drawn over the right edge of the
        // scrollable and would otherwise cover the copy buttons.
        .padding(Padding {
            right: 14.0,
            ..Padding::ZERO
        });
    for (name, value) in fields {
        form = form.push(
            row![
                container(text(name.clone()).size(13).color(muted_text(theme)))
                    .width(Length::Fixed(label_width))
                    .align_x(iced::Alignment::End),
                container(text(value.clone()).size(13).wrapping(Wrapping::Word))
                    .width(Fill)
                    .padding([5, 10])
                    .style(input_like_style),
                button(
                    text(char::from(Bootstrap::Clipboard))
                        .font(BOOTSTRAP_FONT)
                        .size(13),
                )
                .on_press(Message::CopyValue(name.clone(), value.clone()))
                .padding(4)
                .style(ghost_button),
            ]
            .spacing(8)
            .align_y(Center),
        );
    }

    // Header: a tinted glyph, the match number and a short attribute count.
    let heading = row![
        container(
            text(char::from(Bootstrap::CardList))
                .font(BOOTSTRAP_FONT)
                .size(16)
                .color(palette.primary.base.color),
        )
        .padding([8, 9])
        .style(|theme: &Theme| container::Style {
            background: Some(Background::Color(
                theme.extended_palette().primary.weak.color,
            )),
            border: Border {
                radius: 8.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        }),
        column![
            text(title).size(17),
            text(if fields.len() == 1 {
                "1 attribute".to_string()
            } else {
                format!("{} attributes", fields.len())
            })
            .size(12)
            .color(muted_text(theme)),
        ]
        .spacing(2),
    ]
    .spacing(10)
    .align_y(Center);

    let mut header = Row::new().spacing(10).align_y(Center).push(heading);
    header = header.push(Space::with_width(Fill));
    if let Some(attribute) = copy_notice {
        header = header.push(
            row![
                text(char::from(Bootstrap::CheckLg))
                    .font(BOOTSTRAP_FONT)
                    .size(12)
                    .color(palette.success.strong.color),
                text(format!("copied {attribute}"))
                    .size(12)
                    .color(palette.success.strong.color),
            ]
            .spacing(4)
            .align_y(Center),
        );
    }
    header = header.push(
        button(text("Close").size(13))
            .on_press(Message::CloseDetail)
            .padding([6, 12])
            .style(secondary_button),
    );

    // Keep the body height in step with the fixed-height field rows above.
    let body_height = (fields.len() as f32 * FORM_ROW_HEIGHT).clamp(60.0, 440.0);
    let panel = container(
        column![
            header,
            horizontal_rule(1).style(divider_style),
            scrollable(form)
                .height(Length::Fixed(body_height))
                .style(scrollbar_style),
        ]
        .spacing(12),
    )
    .padding(16)
    .width(Length::Fixed(panel_width))
    .style(card_style);

    let overlay = container(opaque(panel))
        .center_x(Fill)
        .center_y(Fill)
        .width(Fill)
        .height(Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.45))),
            ..container::Style::default()
        });

    // Clicking the dimmed backdrop closes the form; `opaque(panel)` keeps
    // presses inside the panel from reaching the backdrop.
    mouse_area(overlay).on_press(Message::CloseDetail).into()
}

/// Read-only field box used by the row detail form.
fn input_like_style(theme: &Theme) -> container::Style {
    let background = if theme.extended_palette().is_dark {
        Color::from_rgba(1.0, 1.0, 1.0, 0.05)
    } else {
        subtle_fill(theme)
    };
    container::Style {
        background: Some(Background::Color(background)),
        border: Border {
            color: hairline(theme),
            width: 1.0,
            radius: RADIUS.into(),
        },
        ..container::Style::default()
    }
}

/// Slim, rounded scrollbar that fades into the panel.
fn scrollbar_style(theme: &Theme, _status: scrollable::Status) -> scrollable::Style {
    let palette = theme.extended_palette();
    let scroller = if palette.is_dark {
        Color::from_rgba(1.0, 1.0, 1.0, 0.22)
    } else {
        Color::from_rgba(0.06, 0.09, 0.16, 0.22)
    };
    let rail = scrollable::Rail {
        background: None,
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        scroller: scrollable::Scroller {
            color: scroller,
            border: Border {
                radius: 4.0.into(),
                ..Border::default()
            },
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
    }
}

/// Divider between the toolbar and the rows. Kept for the table header
/// separator, which sits between the sticky header and the first data row.
fn divider_style(theme: &Theme) -> iced::widget::rule::Style {
    iced::widget::rule::Style {
        color: hairline(theme),
        width: 1,
        radius: 0.0.into(),
        fill_mode: iced::widget::rule::FillMode::Full,
    }
}

/// Alternating background used to stripe the data rows.
fn stripe_style(active: bool) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        if !active {
            return container::Style::default();
        }
        let weak = theme.extended_palette().background.weak.color;
        container::Style {
            background: Some(Background::Color(weak)),
            ..container::Style::default()
        }
    }
}

/// Chip style: a transparent background (the row color shows through) with a
/// border, so a chip never blends into the plain or the striped row background.
/// Chips whose attribute is **indexed** get a success accent, and chips matching
/// the attribute filter get the primary accent.
fn chip_style(theme: &Theme, highlight: bool, indexed: bool) -> container::Style {
    let palette = theme.extended_palette();
    if indexed {
        return container::Style {
            background: Some(Background::Color(tint(
                palette.success.base.color,
                if palette.is_dark { 0.20 } else { 0.12 },
            ))),
            border: Border {
                color: palette.success.strong.color,
                width: 1.5,
                radius: RADIUS.into(),
            },
            ..container::Style::default()
        };
    }
    if highlight {
        return container::Style {
            background: Some(Background::Color(tint(
                palette.primary.base.color,
                if palette.is_dark { 0.22 } else { 0.12 },
            ))),
            border: Border {
                color: palette.primary.strong.color,
                width: 1.5,
                radius: RADIUS.into(),
            },
            ..container::Style::default()
        };
    }
    container::Style {
        background: None,
        border: Border {
            color: hairline(theme),
            width: 1.0,
            radius: RADIUS.into(),
        },
        ..container::Style::default()
    }
}

/// A single chip with an index toggle, a mute icon and a lock icon. `show_name`
/// renders `attribute = value` instead of the value alone; the tooltip always
/// shows the full label, and a click copies the value (a double click opens the
/// row form). A locked chip shows a filled lock instead of the hide icon.
fn chip<'a>(
    header: &'a str,
    value: &'a str,
    row: usize,
    column: usize,
    max_width: f32,
    highlight: bool,
    indexed: bool,
    locked: bool,
    show_name: bool,
) -> Element<'a, Message> {
    let full_label = format!("{header} = {value}");
    let label_text = if show_name {
        full_label.clone()
    } else {
        value.to_string()
    };
    let content_width = (max_width - CHIP_CHROME).max(60.0);
    let clipped = estimate_chip_width(&label_text) > max_width;
    // Cap the label to the chip's share of the line so a wide glyph (or a
    // character-width estimate that undershoots) cannot push the chip past its
    // slot and spill the row: the text wraps inside the chip, and longer
    // values are revealed by the tooltip.
    let label = container(
        text(label_text)
            .size(13)
            .wrapping(Wrapping::WordOrGlyph),
    )
    .max_width(content_width);

    let database = if indexed {
        Bootstrap::DatabaseFill
    } else {
        Bootstrap::Database
    };
    let toggle_index = button(text(char::from(database)).font(BOOTSTRAP_FONT).size(14))
        .on_press(Message::ToggleIndex(column))
        .padding(2)
        .style(ghost_button);

    let mute = button(text(char::from(Bootstrap::EyeSlash)).font(BOOTSTRAP_FONT).size(14))
        .on_press(Message::Mute(column))
        .padding(2)
        .style(ghost_button);

    // A locked column is pinned visible: the filled lock reflects that and the
    // hide icon is dropped, since hiding it is ignored anyway.
    let lock_icon = if locked {
        Bootstrap::LockFill
    } else {
        Bootstrap::Unlock
    };
    let lock = button(text(char::from(lock_icon)).font(BOOTSTRAP_FONT).size(14))
        .on_press(Message::ToggleLock(column))
        .padding(2)
        .style(ghost_button);

    let mut icons = row![label, toggle_index].spacing(6).align_y(Center);
    if !locked {
        icons = icons.push(mute);
    }
    icons = icons.push(lock);

    // Clicking the chip (anywhere but the icons, which capture their own
    // events) copies the cell value to the clipboard.
    let chip = button(
        container(icons)
            .padding([3, 8])
            .style(move |theme| chip_style(theme, highlight, indexed)),
    )
    .on_press(Message::ChipClicked(row, column))
    .padding(0)
    .style(chip_button_style);

    if !clipped {
        return chip.into();
    }

    // Only clipped chips need the overlay; the tooltip reveals the full value
    // that the two-line limit cuts off.
    tooltip(
        chip,
        container(text(full_label).size(12).wrapping(Wrapping::Word))
            .width(Length::Fixed(360.0))
            .padding(8),
        tooltip::Position::FollowCursor,
    )
    .gap(4)
    .padding(0)
    .into()
}

/// Chip wrapper button: invisible apart from a faint accent tint on hover, so
/// the chip keeps its own border and background.
fn chip_button_style(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => {
                Some(Background::Color(palette.primary.weak.color))
            }
            _ => None,
        },
        text_color: palette.background.base.text,
        // Match the chip container's radius so the hover/selected fill does not
        // show square corners behind the rounded chip.
        border: Border {
            radius: RADIUS.into(),
            ..Border::default()
        },
        shadow: Shadow::default(),
    }
}

/// Rough estimate of a chip's width, used to decide whether its label must
/// wrap. Slightly overestimates so chips err on the side of wrapping.
fn estimate_chip_width(label: &str) -> f32 {
    label.chars().count() as f32 * CHAR_WIDTH + CHIP_CHROME
}

/// Read the header row once so chips can be labelled before the first scan.
fn read_headers(path: &Path, delimiter: u8) -> Result<Vec<String>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
    builder.delimiter(delimiter).has_headers(true);
    let mut reader = builder.from_reader(file);

    let headers = reader
        .byte_headers()
        .map_err(|e| format!("cannot read headers of {}: {e}", path.display()))?;

    Ok(headers
        .iter()
        .map(|cell| String::from_utf8_lossy(cell).into_owned())
        .collect())
}

/// Memory-map a file read-only. Returns `None` for a zero-length file, which
/// cannot be mapped.
fn map_file(path: &Path) -> Result<Option<Mmap>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
        .len();
    if len == 0 {
        return Ok(None);
    }
    // SAFETY: the mapping is read-only and the viewer never writes to the
    // mapped file while a scan is running.
    let map = unsafe { Mmap::map(&file) }
        .map_err(|e| format!("cannot memory-map {}: {e}", path.display()))?;
    Ok(Some(map))
}

/// The predicate applied to each data row: an optional regex and an optional
/// per-column mask. When the mask is set, only columns whose entry is `true`
/// are tested (used to skip hidden attributes).
struct Matcher<'a> {
    regex: Option<&'a Regex>,
    visible: Option<&'a [bool]>,
}

impl Matcher<'_> {
    fn is_match(&self, record: &ByteRecord) -> bool {
        let Some(regex) = self.regex else {
            return true;
        };
        record.iter().enumerate().any(|(index, cell)| {
            let searched = match self.visible {
                Some(mask) => mask.get(index).copied().unwrap_or(false),
                None => true,
            };
            searched && regex.is_match(&String::from_utf8_lossy(cell))
        })
    }
}

/// Copy a record into owned strings for display.
fn collect_row(record: &ByteRecord) -> Vec<String> {
    record
        .iter()
        .map(|cell| String::from_utf8_lossy(cell).into_owned())
        .collect()
}

/// Run an already-compiled plan over the CSV. `collect_hits` selects a rule
/// whose rows should be retained (the "show all rows" action); `None` keeps
/// only the bounded sample. `hits_limit` caps the retained rows *per outcome*.
fn run_plan(
    plan: &Plan,
    csv: &Path,
    delimiter: u8,
    collect_hits: Option<usize>,
    hits_limit: usize,
    distinct: bool,
) -> Result<Report, String> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let config = EngineConfig {
        path: csv.to_path_buf(),
        delimiter,
        threads,
        // The viewer keeps row numbers as the row id: there is no id column
        // input, and rules are evaluated in parallel.
        id_idx: None,
        progress: None,
        collect_hits,
        collect_hits_limit: hits_limit,
        collect_distinct: distinct,
    };
    engine::run(plan, &config)
}

/// Release a collected row list without blocking the UI thread. Freeing
/// millions of `RowHit`s (each with several `String`s) can take seconds, so the
/// deallocation is handed to a worker thread instead of stalling `update`.
fn discard_rule_hits(hits: Option<RuleHits>) {
    if let Some(hits) = hits {
        rayon::spawn(move || drop(hits));
    }
}

/// Collect one rule's rows for the grid. The compiled plan is filtered down to
/// that rule before the engine runs, so collecting a rule never re-evaluates
/// the rest of the rule set. At most `limit` rows are retained per outcome, so
/// an outcome view can still show `limit` rows without materializing the whole
/// rule. Returns the rule's rows with their captured CSV cells intact.
fn collect_rule_hits(
    plan: Arc<Plan>,
    rule: usize,
    csv: PathBuf,
    delimiter: u8,
    limit: usize,
    distinct: bool,
) -> Result<RuleHits, String> {
    let compiled = plan
        .rules
        .get(rule)
        .cloned()
        .ok_or_else(|| format!("rule {} not found", rule + 1))?;
    let single = Plan {
        rules: vec![compiled],
    };
    let mut report = run_plan(&single, &csv, delimiter, Some(0), limit, distinct)?;
    let entry = report
        .rules
        .get_mut(0)
        .ok_or_else(|| format!("rule {} not found", rule + 1))?;
    let mut hits = std::mem::take(&mut entry.hits);
    // Keep each row behind an `Arc` so the grid can switch outcome without
    // copying the record.
    let rows = hits
        .iter_mut()
        .map(|hit| Arc::new(std::mem::take(&mut hit.cells)))
        .collect();
    Ok(RuleHits {
        rule,
        cap: limit,
        hits,
        rows,
        passed: entry.rows_passed,
        failed: entry.rows_failed,
        skipped: entry.rows_skipped,
        validation_skipped: entry.rows_validation_skipped,
        distinct,
    })
}

/// Stream the whole file, count every match and keep the first `limit` rows.
/// Runs on a background thread through `Task::perform`.
///
/// `parallel` selects the full-file segmented scan (exact totals, no early
/// exit); otherwise the sequential scan stops as soon as `limit` rows matched.
/// `visible` restricts the search to the columns whose entry is `true`.
fn scan(
    path: PathBuf,
    delimiter: u8,
    pattern: String,
    case_sensitive: bool,
    limit: usize,
    visible: Option<Vec<bool>>,
    parallel: bool,
) -> Result<ScanResult, String> {
    let regex = if pattern.trim().is_empty() {
        None
    } else {
        Some(
            RegexBuilder::new(&pattern)
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(|e| format!("invalid regex: {e}"))?,
        )
    };

    let Some(mmap) = map_file(&path)? else {
        return Ok(ScanResult::empty());
    };
    let bytes: &[u8] = &mmap;
    let matcher = Matcher {
        regex: regex.as_ref(),
        visible: visible.as_deref(),
    };

    if parallel {
        scan_parallel(bytes, delimiter, &matcher, limit)
    } else {
        scan_sequential(bytes, delimiter, &matcher, limit)
    }
}

/// Sequential scan over the memory map. Stops as soon as `limit` rows matched,
/// so the totals are only exact when the scan was not truncated.
fn scan_sequential(
    bytes: &[u8],
    delimiter: u8,
    matcher: &Matcher,
    limit: usize,
) -> Result<ScanResult, String> {
    let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
    builder
        .delimiter(delimiter)
        .has_headers(true)
        .flexible(true);
    let mut reader = builder.from_reader(bytes);

    let mut record = ByteRecord::new();
    let mut rows = Vec::new();
    let mut matched = 0usize;
    let mut truncated = false;
    let mut rows_read = 0usize;

    loop {
        match reader.read_byte_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return Err(format!("error reading CSV: {e}")),
        }
        rows_read += 1;

        if matcher.is_match(&record) {
            matched += 1;
            if rows.len() < limit {
                rows.push(collect_row(&record));
            } else {
                // Stop scanning: only the first `limit` matching rows are shown.
                truncated = true;
                break;
            }
        }
    }

    if truncated {
        // The exact total is unknown because the scan stopped early; report the
        // rows that were actually kept.
        matched = rows.len();
    }
    Ok(ScanResult {
        rows,
        matched,
        truncated,
        rows_read,
        indexed: false,
    })
}

/// Record-aligned byte ranges for a parallel scan over a memory map.
fn segments_for(bytes: &[u8], delimiter: u8, count: usize) -> Result<Vec<(u64, u64)>, String> {
    let mut builder = simd_csv::SeekerBuilder::new();
    builder.delimiter(delimiter).has_headers(true);
    let seeker = builder
        .from_reader(Cursor::new(bytes))
        .map_err(|e| format!("cannot seek CSV: {e}"))?;
    let Some(mut seeker) = seeker else {
        return Ok(Vec::new());
    };
    let ranges = seeker
        .segments(count.max(1))
        .map_err(|e| format!("cannot split CSV: {e}"))?;
    Ok(ranges.into_iter().filter(|(from, to)| to > from).collect())
}

/// Result of scanning one record-aligned segment.
struct SegmentScan {
    rows: Vec<Vec<String>>,
    matched: usize,
    rows_read: usize,
}

fn scan_segment(
    bytes: &[u8],
    delimiter: u8,
    matcher: &Matcher,
    from: u64,
    to: u64,
    limit: usize,
) -> Result<SegmentScan, String> {
    let slice = bytes.get(from as usize..to as usize).unwrap_or_default();
    let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
    builder
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true);
    let mut reader = builder.from_reader(slice);

    let mut record = ByteRecord::new();
    let mut rows = Vec::new();
    let mut matched = 0usize;
    let mut rows_read = 0usize;

    loop {
        match reader.read_byte_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return Err(format!("error reading CSV segment: {e}")),
        }
        rows_read += 1;
        if matcher.is_match(&record) {
            matched += 1;
            // Keep at most `limit` rows per segment: later segments can never
            // contribute to the first `limit` rows in file order.
            if rows.len() < limit {
                rows.push(collect_row(&record));
            }
        }
    }

    Ok(SegmentScan {
        rows,
        matched,
        rows_read,
    })
}

/// Read the whole file in parallel, preserving file order for the rows shown.
/// This never exits early, so `matched` and `rows_read` are exact totals.
fn scan_parallel(
    bytes: &[u8],
    delimiter: u8,
    matcher: &Matcher,
    limit: usize,
) -> Result<ScanResult, String> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let segments = segments_for(bytes, delimiter, threads)?;
    if segments.len() <= 1 {
        // Too small to split: the sequential scan is equivalent.
        return scan_sequential(bytes, delimiter, matcher, limit);
    }

    let per_segment: Vec<SegmentScan> = segments
        .par_iter()
        .map(|&(from, to)| scan_segment(bytes, delimiter, matcher, from, to, limit))
        .collect::<Result<Vec<_>, String>>()?;

    // Merge in file order so the displayed rows keep the file's row order.
    let mut rows = Vec::new();
    let mut matched = 0usize;
    let mut rows_read = 0usize;
    for segment in per_segment {
        matched += segment.matched;
        rows_read += segment.rows_read;
        if rows.len() < limit {
            rows.extend(segment.rows.into_iter().take(limit - rows.len()));
        }
    }

    let truncated = matched > rows.len();
    Ok(ScanResult {
        rows,
        matched,
        truncated,
        rows_read,
        indexed: false,
    })
}

/// Build a prefix index for one column: one `(lowercased value, row byte offset)`
/// entry per data row, then sorted by value. Runs on a background thread.
fn build_index(path: PathBuf, delimiter: u8, column: usize) -> Result<ColumnIndex, String> {
    let Some(mmap) = map_file(&path)? else {
        return Ok(ColumnIndex {
            entries: Vec::new(),
        });
    };
    let bytes: &[u8] = &mmap;

    let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
    builder
        .delimiter(delimiter)
        .has_headers(true)
        .flexible(true);
    let mut reader = builder.from_reader(bytes);
    reader
        .byte_headers()
        .map_err(|e| format!("error reading CSV headers: {e}"))?;

    let mut record = ByteRecord::new();
    let mut entries: Vec<(String, u64)> = Vec::new();
    loop {
        // `position` is the start of the record that is about to be read; after
        // `byte_headers` it points at the first data row.
        let offset = reader.position();
        match reader.read_byte_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return Err(format!("error reading CSV: {e}")),
        }
        let value = record
            .get(column)
            .map(|cell| String::from_utf8_lossy(cell).to_lowercase())
            .unwrap_or_default();
        entries.push((value, offset));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(ColumnIndex { entries })
}

/// Read the row starting at each byte offset, in the given order, up to `limit`.
fn rows_at_offsets(
    bytes: &[u8],
    delimiter: u8,
    offsets: &[u64],
    limit: usize,
) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    let mut record = ByteRecord::new();
    for &offset in offsets.iter().take(limit) {
        let slice = bytes.get(offset as usize..).unwrap_or_default();
        let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
        builder
            .delimiter(delimiter)
            .has_headers(false)
            .flexible(true);
        let mut reader = builder.from_reader(slice);
        match reader.read_byte_record(&mut record) {
            Ok(true) => rows.push(collect_row(&record)),
            Ok(false) => {}
            Err(e) => return Err(format!("error reading CSV row: {e}")),
        }
    }
    Ok(rows)
}

/// Prefix ("beginsWith") search served by the built column indexes. The set of
/// matching rows is known from the indexes, so no file scan is needed and the
/// totals are exact.
fn scan_indexed(
    path: PathBuf,
    delimiter: u8,
    prefix: String,
    indexes: Vec<Arc<ColumnIndex>>,
    limit: usize,
) -> Result<ScanResult, String> {
    let Some(mmap) = map_file(&path)? else {
        return Ok(ScanResult::empty());
    };
    let bytes: &[u8] = &mmap;

    let mut offsets: Vec<u64> = Vec::new();
    for index in &indexes {
        offsets.extend(index.prefix_offsets(&prefix));
    }
    offsets.sort_unstable();
    offsets.dedup();

    let matched = offsets.len();
    let rows = rows_at_offsets(bytes, delimiter, &offsets, limit)?;
    let truncated = matched > rows.len();
    Ok(ScanResult {
        rows,
        matched,
        truncated,
        rows_read: matched,
        indexed: true,
    })
}

/// Tinted pill for a rule's `passed` / `failed` status.
fn status_badge_style(theme: &Theme, status: &str) -> container::Style {
    let palette = theme.extended_palette();
    let (background, text) = if status == "passed" {
        (
            tint(palette.success.base.color, 0.16),
            palette.success.strong.color,
        )
    } else {
        (
            tint(palette.danger.base.color, 0.16),
            palette.danger.strong.color,
        )
    };
    container::Style {
        background: Some(Background::Color(background)),
        border: Border {
            radius: 999.0.into(),
            ..Border::default()
        },
        text_color: Some(text),
        ..container::Style::default()
    }
}

/// A clickable outcome count (`passed N`, `failed N`, …). When `active`, the
/// chip is tinted and outlined in the outcome's colour so the current filter is
/// obvious.
fn filter_button_style(
    theme: &Theme,
    status: button::Status,
    active: bool,
    outcome: RowOutcome,
) -> button::Style {
    let palette = theme.extended_palette();
    let (base, strong) = outcome_colors(theme, outcome);
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: if active {
            Some(Background::Color(tint(
                base,
                if palette.is_dark { 0.28 } else { 0.16 },
            )))
        } else if hovered {
            Some(Background::Color(subtle_fill(theme)))
        } else {
            None
        },
        text_color: if active {
            strong
        } else {
            palette.background.base.text
        },
        border: Border {
            color: if active {
                strong
            } else {
                palette.background.strong.color
            },
            width: 1.0,
            radius: 999.0.into(),
        },
        shadow: Shadow::default(),
    }
}

/// The base (tinted fill) and strong (text/border) colours for a row outcome.
fn outcome_colors(theme: &Theme, outcome: RowOutcome) -> (Color, Color) {
    let palette = theme.extended_palette();
    match outcome {
        RowOutcome::Passed => (palette.success.base.color, palette.success.strong.color),
        RowOutcome::Failed => (palette.danger.base.color, palette.danger.strong.color),
        RowOutcome::Skipped => (
            palette.secondary.base.color,
            palette.secondary.strong.color,
        ),
        RowOutcome::ValidationSkipped => (
            palette.primary.weak.color,
            palette.primary.strong.color,
        ),
    }
}

/// One outcome filter chip for the rules panel.
fn outcome_button<'a>(
    index: usize,
    outcome: RowOutcome,
    label: String,
    active: Option<RowOutcome>,
) -> Element<'a, Message> {
    button(text(label).size(12))
        .on_press(Message::RuleFilterRows(index, outcome))
        .padding([2, 10])
        .style(move |theme, status| {
            filter_button_style(theme, status, active == Some(outcome), outcome)
        })
        .into()
}

/// Surface of one rule card inside the panel: a hairline box that separates
/// rules without competing with the floating panel behind it. The rule whose
/// rows currently fill the grid (or is being collected for it) gets a primary
/// tint so the panel makes the active filter obvious.
fn rule_card_style(theme: &Theme, active: bool) -> container::Style {
    let palette = theme.extended_palette();
    let (background, border_color, border_width) = if active {
        (
            tint(palette.primary.base.color, if palette.is_dark { 0.22 } else { 0.12 }),
            palette.primary.base.color,
            1.5,
        )
    } else {
        (subtle_fill(theme), hairline(theme), 1.0)
    };
    container::Style {
        background: Some(Background::Color(background)),
        border: Border {
            color: border_color,
            width: border_width,
            radius: CARD_RADIUS.into(),
        },
        ..container::Style::default()
    }
}

/// Background of the left rule panel. Tinted a little darker than the floating
/// cards it contains so the two surfaces read as a hierarchy.
fn sidebar_style(theme: &Theme) -> container::Style {
    card_style(theme)
}

fn parse_delimiter(raw: &str) -> Result<u8, String> {
    match raw {
        "\\t" | "tab" | "TAB" => Ok(b'\t'),
        other => {
            let bytes = other.as_bytes();
            if bytes.len() == 1 {
                Ok(bytes[0])
            } else {
                Err(format!("delimiter must be a single byte, got '{other}'"))
            }
        }
    }
}

fn main() -> iced::Result {
    let args = Args::parse();

    // iced selects the compositor from `ICED_BACKEND` (or automatically when it
    // is unset, which is `wgpu` followed by `tiny-skia`).
    match args.backend {
        Backend::Auto => {}
        Backend::Wgpu => std::env::set_var("ICED_BACKEND", "wgpu"),
        Backend::TinySkia => std::env::set_var("ICED_BACKEND", "tiny-skia"),
    }

    iced::application("fview — CSV viewer", Viewer::update, Viewer::view)
        .subscription(Viewer::subscription)
        .theme(Viewer::theme)
        .font(BOOTSTRAP_FONT_BYTES)
        .window_size((1200.0, 820.0))
        .run_with(move || Viewer::new(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_toml() {
        let mut config = Config::default();
        config.profiles.insert(
            "compact".into(),
            ProfileConfig {
                visible: vec!["id".into(), "name".into()],
            },
        );
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("[profiles.compact]"));
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.profiles["compact"].visible, vec!["id", "name"]);
    }

    #[test]
    fn attr_matches_is_case_insensitive_and_empty_never_matches() {
        assert!(attr_matches("NAME", "full_name"));
        assert!(attr_matches("  name ", "full_name"));
        assert!(!attr_matches("", "full_name"));
        assert!(!attr_matches("   ", "full_name"));
        assert!(!attr_matches("age", "full_name"));
    }

    #[test]
    fn hidden_note_pluralizes() {
        assert_eq!(hidden_note(1).matches("attribute").count(), 1);
        assert!(hidden_note(1).contains("1 hidden attribute available"));
        assert!(hidden_note(42).contains("42 hidden attributes available"));
    }

    #[test]
    fn status_text_reports_rows_read() {
        assert_eq!(
            status_text(100, 100, 101, true, false, None),
            "showing first 100 matching rows (more available) · 101 rows read"
        );
        // A full scan knows the exact match total, so it can name it.
        assert_eq!(
            status_text(100, 714, 5000, true, false, None),
            "showing first 100 of 714 matching rows · 5000 rows read"
        );
        assert_eq!(
            status_text(7, 7, 500, false, false, None),
            "7 matching rows of 500 total"
        );
        assert_eq!(status_text(500, 500, 500, false, false, None), "500 rows");
        // Index results are exact and never read the file.
        assert_eq!(
            status_text(100, 714, 714, true, true, None),
            "showing first 100 of 714 matching rows (index prefix)"
        );
        assert_eq!(
            status_text(7, 7, 7, false, true, None),
            "7 matching rows (index prefix)"
        );
        // A completed search appends how long it took.
        assert_eq!(
            status_text(7, 7, 7, false, true, Some(Duration::from_millis(42))),
            "7 matching rows (index prefix) · 42 ms"
        );
        assert_eq!(
            status_text(500, 500, 500, false, false, Some(Duration::from_millis(1500))),
            "500 rows · 1.50 s"
        );
    }

    #[test]
    fn scan_counts_rows_read_and_stops_at_limit() {
        let path = std::env::temp_dir().join(format!("fview-scan-{}.csv", std::process::id()));
        std::fs::write(&path, "a,b\n1,2\n3,4\n5,6\n").unwrap();

        // The limit is hit mid-file, so only part of it is read.
        let limited = scan(path.clone(), b',', String::new(), false, 2, None, false).unwrap();
        assert_eq!(limited.rows.len(), 2);
        assert_eq!(limited.matched, 2);
        assert!(limited.truncated);
        assert_eq!(limited.rows_read, 3);

        // A large enough limit reads the whole file.
        let full = scan(path.clone(), b',', String::new(), false, 10, None, false).unwrap();
        assert_eq!(full.rows.len(), 3);
        assert_eq!(full.matched, 3);
        assert!(!full.truncated);
        assert_eq!(full.rows_read, 3);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scan_respects_the_visible_mask() {
        let path = std::env::temp_dir().join(format!("fview-visible-{}.csv", std::process::id()));
        std::fs::write(&path, "a,b\nfoo,x\nbar,foo\n").unwrap();

        // Without a mask every column is searched: `foo` hits both rows.
        let all = scan(path.clone(), b',', "foo".into(), false, 10, None, false).unwrap();
        assert_eq!(all.rows.len(), 2);

        // With column `b` hidden, row 2 (matched only via `b`) is skipped.
        let masked = scan(
            path.clone(),
            b',',
            "foo".into(),
            false,
            10,
            Some(vec![true, false]),
            false,
        )
        .unwrap();
        assert_eq!(masked.rows.len(), 1);
        assert_eq!(masked.matched, 1);

        // A pattern only present in the hidden column finds nothing.
        let hidden_only = scan(
            path.clone(),
            b',',
            "x".into(),
            false,
            10,
            Some(vec![true, false]),
            false,
        )
        .unwrap();
        assert!(hidden_only.rows.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parallel_scan_matches_sequential_and_reports_exact_totals() {
        let path =
            std::env::temp_dir().join(format!("fview-parallel-{}.csv", std::process::id()));
        let mut data = String::from("id,city\n");
        for i in 0..5000 {
            data.push_str(&format!("{i},city{}\n", i % 7));
        }
        std::fs::write(&path, data).unwrap();

        // Guard the test: if the file cannot be split the parallel path falls
        // back to the sequential one and the exact totals below are vacuous.
        let bytes = std::fs::read(&path).unwrap();
        assert!(segments_for(&bytes, b',', 4).unwrap().len() > 1);

        let sequential =
            scan(path.clone(), b',', "city3".into(), false, 10, None, false).unwrap();
        let parallel = scan(path.clone(), b',', "city3".into(), false, 10, None, true).unwrap();

        // Same rows, in the same (file) order.
        assert_eq!(sequential.rows, parallel.rows);
        // The full parallel scan reads everything and knows the exact totals.
        assert_eq!(parallel.rows_read, 5000);
        assert_eq!(parallel.matched, (0..5000).filter(|i| i % 7 == 3).count());
        assert!(parallel.truncated);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn index_prefix_search_returns_file_order_offsets() {
        let index = ColumnIndex {
            entries: vec![
                ("apple".into(), 30),
                ("apricot".into(), 10),
                ("banana".into(), 20),
            ],
        };
        // Entries are sorted by value, so the result is re-sorted by position.
        assert_eq!(index.prefix_offsets("ap"), vec![10, 30]);
        assert_eq!(index.prefix_offsets("ban"), vec![20]);
        // Matching is case-insensitive.
        assert_eq!(index.prefix_offsets("APPLE"), vec![30]);
        assert!(index.prefix_offsets("zzz").is_empty());
    }

    #[test]
    fn build_index_offsets_resolve_to_the_right_rows() {
        let path = std::env::temp_dir().join(format!("fview-index-{}.csv", std::process::id()));
        std::fs::write(&path, "name,city\nAlice,Paris\nBob,Lyon\nAnna,Nice\n").unwrap();
        let bytes = std::fs::read(&path).unwrap();

        let index = build_index(path.clone(), b',', 0).unwrap();
        // Sorted by lowercased value: "alice", "anna", "bob".
        let offsets = index.prefix_offsets("an");
        assert_eq!(offsets.len(), 1);
        let rows = rows_at_offsets(&bytes, b',', &offsets, 10).unwrap();
        assert_eq!(rows, vec![vec!["Anna".to_string(), "Nice".to_string()]]);

        // A broader prefix returns every matching row in file order.
        let offsets = index.prefix_offsets("a");
        let rows = rows_at_offsets(&bytes, b',', &offsets, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "Alice");
        assert_eq!(rows[1][0], "Anna");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scan_indexed_uses_a_prefix_query_over_the_indexed_column() {
        let path = std::env::temp_dir().join(format!("fview-scanidx-{}.csv", std::process::id()));
        std::fs::write(&path, "name,city\nAlice,Paris\nBob,Lyon\nAnna,Nice\n").unwrap();

        let index = Arc::new(build_index(path.clone(), b',', 0).unwrap());
        let result = scan_indexed(
            path.clone(),
            b',',
            "An".into(),
            vec![Arc::clone(&index)],
            10,
        )
        .unwrap();
        assert!(result.indexed);
        assert_eq!(result.matched, 1);
        assert_eq!(result.rows, vec![vec!["Anna".to_string(), "Nice".to_string()]]);

        // Prefix search anchors at the start: "li" occurs in "Alice" but no
        // indexed value starts with it.
        let none = scan_indexed(path.clone(), b',', "li".into(), vec![index], 10).unwrap();
        assert_eq!(none.matched, 0);
        assert!(none.rows.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stripe_height_grows_with_line_count() {
        // Virtual offsets rely on a stable, positive height.
        assert!(stripe_height(1) > 0.0);
        assert!(stripe_height(3) > stripe_height(2));
        assert!(stripe_height(2) > stripe_height(1));
        // Degenerate input must not collapse to zero height.
        assert_eq!(stripe_height(0), stripe_height(1));
    }

    #[test]
    fn chip_lines_fill_each_line_before_wrapping() {
        // Three 100px chips with 8px spacing: two fit in 260, the third wraps.
        let available = 260.0;
        let widths = [100.0, 100.0, 100.0];
        let lines = chip_lines(&widths, available);
        assert_eq!(lines, vec![0..2, 2..3]);
        assert_eq!(chip_line_count(widths.iter().copied(), available), lines.len());

        // A chip wider than the whole line still occupies exactly one line.
        let wide = [available + 50.0];
        assert_eq!(chip_line_count(wide.iter().copied(), available), 1);
        assert_eq!(chip_lines(&wide, available), vec![0..1]);

        // An empty row still reports one line so the stripe keeps a height.
        assert_eq!(chip_line_count(std::iter::empty(), available), 1);
    }

    #[test]
    fn chip_line_count_matches_chip_lines() {
        let cases: [[f32; 4]; 4] = [
            [120.0, 40.0, 300.0, 90.0],
            [500.0, 500.0, 500.0, 500.0],
            [10.0, 10.0, 10.0, 10.0],
            [0.0, 0.0, 0.0, 0.0],
        ];
        let available = 420.0;
        for widths in cases {
            let ranges = chip_lines(&widths, available);
            assert_eq!(
                chip_line_count(widths.iter().copied(), available),
                ranges.len(),
                "widths {widths:?}"
            );
            // Ranges must cover every chip exactly once, in order.
            let covered: Vec<usize> = ranges.iter().flat_map(|range| range.clone()).collect();
            assert_eq!(covered, (0..widths.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn rule_view_can_render_full_rows_from_engine() {
        // The engine captures each hit's full row in the same parallel pass, so
        // the GUI's rule view no longer reads the file a second time. This
        // checks the captured cells line up with the Hit's identity.
        let dir = std::env::temp_dir().join(format!("fview-rows-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        std::fs::write(&path, "id,name\n1,Alice\n2,Bob\n3,Cara\n").unwrap();

        let rules_path = dir.join("r.vl");
        std::fs::write(
            &rules_path,
            "rule \"name exists\" {\n  left = id\n  right = name\n  mapping = none\n}\n",
        )
        .unwrap();
        let headers = engine::read_headers(&path, b',').unwrap();
        let program = dsl::load_file(&rules_path).unwrap();
        let plan = Arc::new(rules::compile(program, &headers).unwrap());
        let hits = collect_rule_hits(plan, 0, path, b',', usize::MAX, false).unwrap();
        assert_eq!(hits.hits.len(), 3);
        assert_eq!(*hits.rows[0], vec!["1".to_string(), "Alice".to_string()]);
        assert_eq!(*hits.rows[2], vec!["3".to_string(), "Cara".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collecting_one_rule_ignores_the_others() {
        // The plan holds two rules but only the requested one is evaluated, so
        // switching rules never re-runs the whole rule set.
        let dir = std::env::temp_dir().join(format!("fview-single-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        std::fs::write(&path, "id,a,b\n1,x,x\n2,y,z\n").unwrap();
        let rules_path = dir.join("r.vl");
        std::fs::write(
            &rules_path,
            "rule \"a\" {\n  left = a\n  right = b\n  mapping = none\n}\n\
             rule \"id\" {\n  left = id\n  right = a\n  mapping = none\n}\n",
        )
        .unwrap();
        let headers = engine::read_headers(&path, b',').unwrap();
        let program = dsl::load_file(&rules_path).unwrap();
        let plan = Arc::new(rules::compile(program, &headers).unwrap());

        let a =
            collect_rule_hits(Arc::clone(&plan), 0, path.clone(), b',', usize::MAX, false).unwrap();
        assert_eq!(a.hits.len(), 2);
        assert_eq!(a.hits.iter().filter(|hit| hit.passed()).count(), 1);
        assert_eq!(a.passed, 1);
        assert_eq!(a.failed, 1);

        let id = collect_rule_hits(plan, 1, path, b',', usize::MAX, false).unwrap();
        assert_eq!(id.hits.len(), 2);
        assert!(id.hits.iter().all(|hit| !hit.passed()));
        assert_eq!(id.failed, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn distinct_collection_caps_each_condition_and_groups_the_grid() {
        // One condition repeated 100 times plus five distinct ones. The plain
        // collection would spend its whole budget on the repeated condition;
        // the distinct mode keeps a tenth of the budget per condition.
        let dir = std::env::temp_dir().join(format!("fview-distinct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        let mut csv = String::from("id,a,b\n");
        for index in 0..100 {
            csv.push_str(&format!("{index},x,y\n"));
        }
        for index in 100..105 {
            csv.push_str(&format!("{index},p{index},q{index}\n"));
        }
        std::fs::write(&path, csv).unwrap();
        let rules_path = dir.join("r.vl");
        std::fs::write(
            &rules_path,
            "rule \"eq\" {\n  left = a\n  right = b\n  mapping = none\n}\n",
        )
        .unwrap();
        let headers = engine::read_headers(&path, b',').unwrap();
        let program = dsl::load_file(&rules_path).unwrap();
        let plan = Arc::new(rules::compile(program, &headers).unwrap());

        // limit 100 -> 10 rows per distinct condition.
        let hits = collect_rule_hits(plan, 0, path, b',', 100, true).unwrap();
        let repeated = hits
            .hits
            .iter()
            .filter(|hit| hit.left == "x" && hit.right == "y")
            .count();
        assert_eq!(repeated, 10, "the repeated condition is capped at limit/10");
        assert_eq!(hits.failed, 105);

        let mut viewer = test_viewer();
        viewer.rules.distinct = true;
        viewer.rules.hits_filter = Some(RowOutcome::Failed);
        viewer.rules.hits = Some(hits);
        let _ = viewer.apply_rule_view();
        // x|y plus the five distinct conditions.
        assert_eq!(viewer.rows.len(), 15);
        let headings = viewer
            .row_groups
            .iter()
            .filter(|group| group.is_some())
            .count();
        assert_eq!(headings, 6, "each distinct condition gets one heading");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clicking_another_rule_queues_while_collecting() {
        let mut viewer = test_viewer();
        viewer.path = Some(PathBuf::from("/tmp/fview-queued.csv"));
        viewer.rules.plan = Some(Arc::new(Plan { rules: Vec::new() }));
        viewer.rules.collecting = true;
        viewer.rules.pending_rule = Some(0);

        // A click on a second rule is queued instead of launching a parallel
        // whole-file evaluation.
        let _ = viewer.update(Message::RuleAllRows(1));
        assert_eq!(viewer.rules.queued_rule, Some((1, None)));
        assert_eq!(viewer.rules.pending_rule, Some(0));

        // Clicking the rule already being collected drops the queued one.
        let _ = viewer.update(Message::RuleFilterRows(0, RowOutcome::Failed));
        assert_eq!(viewer.rules.queued_rule, None);
        assert_eq!(viewer.rules.hits_filter, Some(RowOutcome::Failed));
    }

    /// A minimal viewer with three headers and no file, for testing the
    /// attribute visibility state machine.
    fn test_viewer() -> Viewer {
        let (mut viewer, _task) = Viewer::new(Args {
            path: None,
            delimiter: ",".into(),
            case_sensitive: false,
            limit: 100,
            backend: Backend::TinySkia,
        });
        viewer.headers = vec!["a".into(), "b".into(), "c".into()];
        viewer
    }

    #[test]
    fn rule_view_filtering_shares_rows_instead_of_copying() {
        let mut viewer = test_viewer();
        let row = Arc::new(vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        viewer.rules.hits = Some(RuleHits {
            rule: 0,
            cap: 100,
            distinct: false,
            hits: vec![RowHit {
                id: "1".into(),
                row: None,
                left: "a".into(),
                right: "b".into(),
                expected: None,
                outcome: RowOutcome::Passed,
                cells: Vec::new(),
            }],
            rows: vec![Arc::clone(&row)],
            passed: 1,
            failed: 0,
            skipped: 0,
            validation_skipped: 0,
        });
        viewer.rules.hits_filter = Some(RowOutcome::Passed);
        let _ = viewer.apply_rule_view();
        assert_eq!(viewer.rows.len(), 1);
        // Switching outcome re-uses the cached record: same allocation, so the
        // filter click does not deep-copy every row.
        assert!(Arc::ptr_eq(&viewer.rows[0], &row));
    }

    #[test]
    fn changing_limit_keeps_the_rule_filter() {
        let mut viewer = test_viewer();
        viewer.rules.hits = Some(RuleHits {
            rule: 0,
            cap: 100,
            distinct: false,
            hits: (0..5)
                .map(|index| RowHit {
                    id: index.to_string(),
                    row: None,
                    left: "a".into(),
                    right: "b".into(),
                    expected: None,
                    outcome: RowOutcome::Passed,
                    cells: Vec::new(),
                })
                .collect(),
            rows: (0..5)
                .map(|index| Arc::new(vec![format!("v{index}"), "b".into(), "c".into()]))
                .collect(),
            passed: 5,
            failed: 0,
            skipped: 0,
            validation_skipped: 0,
        });
        viewer.rules.hits_filter = Some(RowOutcome::Passed);
        viewer.rules.view_active = true;

        let _ = viewer.update(Message::LimitSelected(2));

        // The rule/outcome filter survives a limit change; only the number of
        // displayed rows shrinks.
        assert!(viewer.rules.view_active);
        assert_eq!(viewer.rows.len(), 2);
        assert_eq!(viewer.matched, 5);
        assert!(viewer.truncated);
    }

    #[test]
    fn locked_columns_survive_mute_all() {
        let mut viewer = test_viewer();
        viewer.locked.insert(1);

        let _ = viewer.update(Message::MuteAll);
        assert!(!viewer.muted.contains(&1), "a locked column stays visible");
        assert!(viewer.muted.contains(&0));
        assert!(viewer.muted.contains(&2));

        // Hiding a locked column directly is ignored too.
        let _ = viewer.update(Message::Mute(1));
        assert!(!viewer.muted.contains(&1));

        // Unlocking keeps it visible; hiding it is a separate action.
        let _ = viewer.update(Message::ToggleLock(1));
        assert!(!viewer.locked.contains(&1));
        assert!(!viewer.muted.contains(&1));
    }

    #[test]
    fn locked_columns_survive_profiles_and_rule_attrs() {
        let mut viewer = test_viewer();
        viewer.locked.insert(1);
        viewer.profiles.insert(
            "p".into(),
            ProfileConfig {
                visible: vec!["a".into(), "c".into()],
            },
        );

        // The profile only lists a and c, but b is locked so it stays visible.
        viewer.apply_profile("p");
        assert!(viewer.muted.is_empty());

        // "rule attributes only" keeps the locked column as well.
        viewer.rules.attrs_only = true;
        viewer.rules.hits = Some(RuleHits {
            rule: 0,
            cap: 100,
            distinct: false,
            hits: Vec::new(),
            rows: Vec::new(),
            passed: 0,
            failed: 0,
            skipped: 0,
            validation_skipped: 0,
        });
        viewer.rules.view_active = true;
        viewer.sync_rule_attrs();
        assert!(!viewer.muted.contains(&1));
    }
}

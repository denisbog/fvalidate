//! Optional egui-based GUI: grep a big CSV file and browse the matching rows.
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
//! The look and feel follows the PrintCraft egui shell: a neutral chrome, white
//! panels, a single blue accent, rounded corners and Lucide icons. The palette
//! is defined once in the `theme` module and every custom widget reads it.
//!
//! Layout:
//! * the top bar holds the regex filter and the profile controls;
//! * a second bar holds the attribute filter and, when attributes are hidden,
//!   a chip per hidden attribute (click a chip to show the attribute again).
//!   Hidden chips are sorted alphabetically so large attribute lists stay
//!   navigable; the attribute filter narrows the hidden list to the matching
//!   names (and highlights the matching chips in the main view);
//! * every matching row is rendered as a set of `attribute = value` chips, each
//!   with an index icon that builds/drops a prefix index, a mute icon that hides
//!   that attribute from all rows and moves its name into the top bar, and a
//!   lock icon that pins the attribute so it always stays visible (mute,
//!   mute-all, profiles and "rule attributes only" all leave locked columns
//!   alone).
//!
//! Scanning: the filter is **debounced** (a scan starts ~180 ms after the last
//! keystroke, and an unchanged pattern is never re-scanned). The file is
//! memory-mapped read-only. Two opt-in checkboxes in the top bar change the
//! scan: **visible only** searches just the attributes that are currently
//! shown, and **parallel** reads the whole file in record-aligned segments
//! across all cores, which yields exact row/match totals but never exits early.
//!
//! Display and indexing: a **table** checkbox renders the matches as a table of
//! the visible attributes instead of chips. Each chip, and each table header,
//! carries an index button that builds (or drops) a per-column prefix
//! **index**, a mute button that hides the attribute and a lock button that
//! pins it visible; indexed attributes are highlighted (green background,
//! filled icon) in both views. While an index exists and the **index** checkbox
//! is on, a non-empty filter becomes a case-insensitive `beginsWith` prefix
//! query over the indexed columns — served straight from the index, with exact
//! totals and no file scan. Unchecking **index** (or using `--case-sensitive`)
//! falls back to the regex. Chips show only the cell value by default; the
//! **attribute names** checkbox brings back the `attribute = value` label.
//! Clicking a chip copies its value (a tooltip reveals values clipped by the
//! one-line limit, and a double click opens the row form), and clicking a table
//! row — or the empty part of a chip row — opens the form directly. The form
//! closes with its button, the Escape key, or a click on the backdrop. Escape
//! also clears the regex or attribute filter the user was last editing.
//!
//! Profiles: the set of currently visible attributes can be saved under a name
//! and re-applied later. Profiles are persisted as TOML in the platform config
//! directory (`<config>/fview/profiles.toml`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use fast_csv::dsl;
use fast_csv::engine::{self, EngineConfig};
use fast_csv::report::{Report, RowHit, RowOutcome, RuleReport};
use fast_csv::rules::{self, Plan};
use memmap2::Mmap;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use simd_csv::ByteRecord;

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Rect, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2,
};

const BUFFER_CAPACITY: usize = 64 * 1024;
/// How often the debounce timer is polled while a filter edit is pending.
const DEBOUNCE_TICK_MS: u64 = 50;
/// Quiet period after the last filter keystroke before a scan is started.
const DEBOUNCE_QUIET_MS: u64 = 180;
/// Fixed height of one table row in the table view.
const TABLE_ROW_HEIGHT: f32 = 26.0;
/// Minimum width of a table column. The table grows horizontally instead of
/// squeezing columns below this.
const TABLE_CELL_MIN_WIDTH: f32 = 160.0;
/// Spacing between chips, in px.
const CHIP_SPACING: f32 = 8.0;
/// Non-text width of a chip: padding + index icon + mute icon + lock icon +
/// inner spacing.
const CHIP_CHROME: f32 = 96.0;
/// Height of one line of chip text plus the chip's vertical padding.
const CHIP_LINE_BOX: f32 = 26.0;
/// Vertical padding of a data stripe.
const STRIPE_PADDING: f32 = 5.0;
/// Horizontal padding of a data stripe.
const STRIPE_PADDING_H: f32 = 12.0;
/// Padding of the content area that holds the rules sidebar and the grid.
const CONTENT_PADDING: f32 = 10.0;
/// Right padding of the row list, keeping chips clear of the floating scrollbar.
const LIST_RIGHT_PADDING: f32 = 14.0;
/// Spacing between the chip lines inside a stripe.
const CHIP_LINE_SPACING: f32 = 6.0;
/// Rows rendered above and below the viewport so scrolling does not flash gaps.
const OVERSCAN_ROWS: usize = 3;
/// Rough width of one character at size 13, used to decide chip wrapping.
const CHAR_WIDTH: f32 = 7.2;
/// Corner radius shared by cards, inputs and buttons.
const RADIUS: u8 = 6;
/// Fixed height of the status line.
const STATUS_HEIGHT: f32 = 24.0;
/// Fixed height of the toolbar. The top panels are explicitly sized so the
/// central grid can never overlap them (an auto-sized `Panel::top` only reserves
/// `interact_size` on the first frame and lags when its content grows).
const TOOLBAR_HEIGHT: f32 = 46.0;
/// Row-count choices offered by the "rows" drop-down.
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
    /// When the current rule-row collection started, and how long it took.
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
            None => self.passed + self.failed + self.skipped + self.validation_skipped,
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
/// panel built by `rules_panel`.
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

/// Height reserved for one line of chips.
fn chip_line_box() -> f32 {
    CHIP_LINE_BOX
}

/// Fixed height of a data stripe showing `lines` lines of chips.
fn stripe_height(lines: usize) -> f32 {
    let lines = lines.max(1);
    lines as f32 * chip_line_box() + (lines - 1) as f32 * CHIP_LINE_SPACING + 2.0 * STRIPE_PADDING
}

/// Filter box the user last typed in, so Escape clears the expected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterFocus {
    Results,
    Attributes,
}

/// The design tokens and egui style, adapted from the PrintCraft shell: neutral
/// chrome, white panels, one blue accent; the dark theme keeps the same
/// hierarchy. Every custom widget reads `Tokens::get`.
mod theme {
    use super::{Color32, FontId};
    use egui::{CornerRadius, FontData, FontDefinitions, FontFamily, Stroke, Visuals};
    use std::sync::Arc;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
    pub enum ThemeKind {
        #[default]
        Light,
        Dark,
    }

    #[derive(Clone, Copy, Debug)]
    #[allow(dead_code)]
    pub struct Tokens {
        pub kind: ThemeKind,
        pub titlebar: Color32,
        pub chrome: Color32,
        pub panel: Color32,
        pub pasteboard: Color32,
        pub card: Color32,
        pub border: Color32,
        pub divider: Color32,
        pub text: Color32,
        pub text_muted: Color32,
        pub text_faint: Color32,
        pub icon: Color32,
        pub hover: Color32,
        pub pressed: Color32,
        pub selected: Color32,
        pub accent: Color32,
        pub accent_text: Color32,
        pub accent_soft: Color32,
        pub field: Color32,
        pub success: Color32,
        pub success_soft: Color32,
        pub danger: Color32,
        pub danger_soft: Color32,
        pub radius: u8,
    }

    impl Tokens {
        pub fn for_kind(kind: ThemeKind) -> Self {
            match kind {
                ThemeKind::Light => Self {
                    kind,
                    titlebar: Color32::from_rgb(0xE9, 0xE9, 0xEB),
                    chrome: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                    panel: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                    pasteboard: Color32::from_rgb(0xF1, 0xF1, 0xF3),
                    card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                    border: Color32::from_rgb(0xDA, 0xDA, 0xDE),
                    divider: Color32::from_rgb(0xE8, 0xE8, 0xEB),
                    text: Color32::from_rgb(0x22, 0x22, 0x26),
                    text_muted: Color32::from_rgb(0x5E, 0x5E, 0x66),
                    text_faint: Color32::from_rgb(0x6B, 0x6B, 0x73),
                    icon: Color32::from_rgb(0x44, 0x44, 0x4B),
                    hover: Color32::from_rgb(0xF0, 0xF0, 0xF3),
                    pressed: Color32::from_rgb(0xE4, 0xE4, 0xE9),
                    selected: Color32::from_rgb(0xE6, 0xEE, 0xFD),
                    accent: Color32::from_rgb(0x1B, 0x63, 0xE0),
                    accent_text: Color32::from_rgb(0x17, 0x55, 0xC4),
                    accent_soft: Color32::from_rgb(0xE3, 0xEC, 0xFD),
                    field: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                    success: Color32::from_rgb(0x0B, 0x7A, 0x55),
                    success_soft: Color32::from_rgb(0xDC, 0xF5, 0xEA),
                    danger: Color32::from_rgb(0xC0, 0x2A, 0x2A),
                    danger_soft: Color32::from_rgb(0xFD, 0xE7, 0xE7),
                    radius: 6,
                },
                ThemeKind::Dark => Self {
                    kind,
                    titlebar: Color32::from_rgb(0x1B, 0x1B, 0x1E),
                    chrome: Color32::from_rgb(0x26, 0x26, 0x2A),
                    panel: Color32::from_rgb(0x26, 0x26, 0x2A),
                    pasteboard: Color32::from_rgb(0x19, 0x19, 0x1C),
                    card: Color32::from_rgb(0x2E, 0x2E, 0x33),
                    border: Color32::from_rgb(0x3C, 0x3C, 0x43),
                    divider: Color32::from_rgb(0x33, 0x33, 0x39),
                    text: Color32::from_rgb(0xEC, 0xEC, 0xEF),
                    text_muted: Color32::from_rgb(0xAE, 0xAE, 0xB6),
                    text_faint: Color32::from_rgb(0x97, 0x97, 0x9E),
                    icon: Color32::from_rgb(0xD4, 0xD4, 0xDA),
                    hover: Color32::from_rgb(0x34, 0x34, 0x3A),
                    pressed: Color32::from_rgb(0x3E, 0x3E, 0x45),
                    selected: Color32::from_rgb(0x23, 0x3A, 0x63),
                    accent: Color32::from_rgb(0x4B, 0x8B, 0xF5),
                    accent_text: Color32::from_rgb(0x8C, 0xB6, 0xFA),
                    accent_soft: Color32::from_rgb(0x24, 0x36, 0x57),
                    field: Color32::from_rgb(0x1E, 0x1E, 0x22),
                    success: Color32::from_rgb(0x4A, 0xD1, 0x9B),
                    success_soft: Color32::from_rgb(0x1B, 0x3A, 0x30),
                    danger: Color32::from_rgb(0xF0, 0x6B, 0x6B),
                    danger_soft: Color32::from_rgb(0x3D, 0x22, 0x24),
                    radius: 6,
                },
            }
        }

        pub fn get(ctx: &egui::Context) -> Self {
            ctx.data(|d| d.get_temp::<Tokens>(egui::Id::new("fview-theme")))
                .unwrap_or_else(|| Self::for_kind(ThemeKind::Light))
        }

        pub fn dark(&self) -> bool {
            self.kind == ThemeKind::Dark
        }
    }

    /// Install Inter (the PrintCraft UI face) plus egui's own defaults.
    pub fn install_fonts(ctx: &egui::Context) {
        ctx.set_fonts(font_definitions());
    }

    fn font_definitions() -> FontDefinitions {
        let mut fonts = FontDefinitions::default();
        let mut add = |name: &str, bytes: &'static [u8]| {
            fonts
                .font_data
                .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
        };
        add(
            "Inter",
            include_bytes!("../../assets/fonts/Inter-Regular.ttf"),
        );
        add(
            "Inter-Medium",
            include_bytes!("../../assets/fonts/Inter-Medium.ttf"),
        );
        add(
            "Inter-SemiBold",
            include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"),
        );
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "Inter".to_owned());
        let fallback: Vec<String> = fonts.families[&FontFamily::Proportional].clone();
        for (family, primary) in [("medium", "Inter-Medium"), ("semibold", "Inter-SemiBold")] {
            let mut stack = vec![primary.to_owned()];
            stack.extend(fallback.iter().cloned());
            fonts.families.insert(FontFamily::Name(family.into()), stack);
        }
        fonts
    }

    pub fn regular(size: f32) -> FontId {
        FontId::proportional(size)
    }
    pub fn medium(size: f32) -> FontId {
        FontId::new(size, FontFamily::Name("medium".into()))
    }
    pub fn semibold(size: f32) -> FontId {
        FontId::new(size, FontFamily::Name("semibold".into()))
    }

    pub fn apply(ctx: &egui::Context, kind: ThemeKind) {
        let t = Tokens::for_kind(kind);
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("fview-theme"), t));
        let mut v = if t.dark() { Visuals::dark() } else { Visuals::light() };
        v.panel_fill = t.panel;
        v.window_fill = t.card;
        v.window_stroke = Stroke::new(1.0, t.border);
        v.extreme_bg_color = t.field;
        v.faint_bg_color = t.hover;
        v.selection.bg_fill = t.accent_soft;
        v.selection.stroke = Stroke::new(1.0, t.accent);
        v.hyperlink_color = t.accent_text;
        v.override_text_color = Some(t.text);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        v.window_shadow = egui::Shadow {
            offset: [0, 8],
            blur: 28,
            spread: 0,
            color: Color32::from_black_alpha(if t.dark() { 110 } else { 38 }),
        };
        v.popup_shadow = egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(if t.dark() { 90 } else { 30 }),
        };
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(t.radius);
            w.fg_stroke.color = t.text;
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, t.divider);
        v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        v.widgets.inactive.bg_fill = t.field;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, t.border);
        v.widgets.hovered.weak_bg_fill = t.hover;
        v.widgets.hovered.bg_fill = t.hover;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, t.border);
        v.widgets.active.weak_bg_fill = t.pressed;
        v.widgets.active.bg_fill = t.pressed;
        v.widgets.open.weak_bg_fill = t.hover;
        v.text_options.subpixel_binning = false;
        ctx.set_visuals(v);
        ctx.global_style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
            s.spacing.button_padding = egui::vec2(10.0, 5.0);
            s.spacing.menu_margin = egui::Margin::same(6);
            s.spacing.scroll.bar_width = 8.0;
            s.spacing.scroll.floating = true;
            s.text_styles.insert(egui::TextStyle::Body, regular(13.0));
            s.text_styles.insert(egui::TextStyle::Button, regular(13.0));
            s.text_styles.insert(egui::TextStyle::Small, regular(11.0));
            s.text_styles.insert(egui::TextStyle::Heading, semibold(17.0));
            s.interaction.tooltip_delay = 0.35;
        });
    }
}

/// Lucide icons (ISC licence, https://lucide.dev), embedded and tinted at runtime.
/// The SVG bodies are inlined here so the viewer stays a single source file.
/// See `assets/icons/LICENSE-lucide.txt` for the licence text.
mod icons {
    use std::collections::HashMap;
    use std::sync::{Arc, OnceLock};

    use egui::{Color32, Rect, Response, Sense, Ui, Vec2};

    use super::theme::Tokens;

    /// Icon name → SVG path markup (no `<svg>` wrapper; the stroke is white so
    /// `Image::tint` can recolour it).
    const BODIES: &[(&str, &str)] = &[
        (
            "file-spreadsheet",
            r#"<path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z"/><path d="M14 2v5a1 1 0 0 0 1 1h5"/><path d="M8 13h2"/><path d="M14 13h2"/><path d="M8 17h2"/><path d="M14 17h2"/>"#,
        ),
        (
            "folder-open",
            r#"<path d="m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2"/>"#,
        ),
        (
            "search",
            r#"<path d="m21 21-4.34-4.34"/><circle cx="11" cy="11" r="8"/>"#,
        ),
        (
            "list-checks",
            r#"<path d="M13 5h8"/><path d="M13 12h8"/><path d="M13 19h8"/><path d="m3 17 2 2 4-4"/><path d="m3 7 2 2 4-4"/>"#,
        ),
        (
            "columns-3",
            r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M9 3v18"/><path d="M15 3v18"/>"#,
        ),
        (
            "eye",
            r#"<path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0"/><circle cx="12" cy="12" r="3"/>"#,
        ),
        (
            "eye-off",
            r#"<path d="M10.733 5.076a10.744 10.744 0 0 1 11.205 6.575 1 1 0 0 1 0 .696 10.747 10.747 0 0 1-1.444 2.49"/><path d="M14.084 14.158a3 3 0 0 1-4.242-4.242"/><path d="M17.479 17.499a10.75 10.75 0 0 1-15.417-5.151 1 1 0 0 1 0-.696 10.75 10.75 0 0 1 4.446-5.143"/><path d="m2 2 20 20"/>"#,
        ),
        (
            "lock",
            r#"<rect width="18" height="11" x="3" y="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>"#,
        ),
        (
            "lock-open",
            r#"<rect width="18" height="11" x="3" y="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 9.9-1"/>"#,
        ),
        (
            "copy",
            r#"<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>"#,
        ),
        ("check", r#"<path d="M20 6 9 17l-5-5"/>"#),
        ("x", r#"<path d="M18 6 6 18"/><path d="m6 6 12 12"/>"#),
        (
            "triangle-alert",
            r#"<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"/><path d="M12 9v4"/><path d="M12 17h.01"/>"#,
        ),
        (
            "clock-3",
            r#"<circle cx="12" cy="12" r="10"/><path d="M12 6v6h4"/>"#,
        ),
        ("chevron-down", r#"<path d="m6 9 6 6 6-6"/>"#),
        ("chevron-right", r#"<path d="m9 18 6-6-6-6"/>"#),
        (
            "grid-3x3",
            r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M3 9h18"/><path d="M3 15h18"/><path d="M9 3v18"/><path d="M15 3v18"/>"#,
        ),
        (
            "sun",
            r#"<circle cx="12" cy="12" r="4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.93 4.93 1.41 1.41"/><path d="m17.66 17.66 1.41 1.41"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.34 17.66-1.41 1.41"/><path d="m19.07 4.93-1.41 1.41"/>"#,
        ),
        (
            "moon",
            r#"<path d="M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"/>"#,
        ),
        ("table", r#"<path d="M12 3v18"/><rect width="18" height="18" x="3" y="3" rx="2"/><path d="M3 9h18"/><path d="M3 15h18"/>"#),
        (
            "settings-2",
            r#"<path d="M14 17H5"/><path d="M19 7h-9"/><circle cx="17" cy="17" r="3"/><circle cx="7" cy="7" r="3"/>"#,
        ),
    ];

    fn rendered() -> &'static HashMap<&'static str, Arc<[u8]>> {
        static MAP: OnceLock<HashMap<&'static str, Arc<[u8]>>> = OnceLock::new();
        MAP.get_or_init(|| {
            BODIES
                .iter()
                .map(|(name, body)| {
                    let svg = format!(
                        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"#ffffff\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\">{body}</svg>"
                    );
                    (*name, Arc::from(svg.into_bytes().into_boxed_slice()))
                })
                .collect()
        })
    }

    pub fn image(name: &str, size: f32, tint: Color32) -> egui::Image<'static> {
        let (key, bytes) = match rendered().get(name) {
            Some(bytes) => (name, bytes.clone()),
            None => ("search", rendered()["search"].clone()),
        };
        egui::Image::from_bytes(
            format!("bytes://fview/{key}.svg"),
            egui::load::Bytes::Shared(bytes),
        )
        .fit_to_exact_size(Vec2::splat(size))
        .tint(tint)
    }

    pub fn paint(ui: &Ui, rect: Rect, name: &str, size: f32, tint: Color32) {
        image(name, size, tint).paint_at(ui, Rect::from_center_size(rect.center(), Vec2::splat(size)));
    }

    /// Square icon button: transparent until hovered; `selected` gets the accent
    /// treatment.
    pub fn button(ui: &mut Ui, name: &str, box_size: f32, selected: bool, tooltip: &str) -> Response {
        let t = Tokens::get(ui.ctx());
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(box_size), Sense::click());
        let label = if tooltip.is_empty() { name } else { tooltip };
        resp.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), selected, label)
        });
        if selected {
            ui.painter().rect_filled(rect, t.radius, t.accent_soft);
        } else if resp.is_pointer_button_down_on() {
            ui.painter().rect_filled(rect, t.radius, t.pressed);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, t.radius, t.hover);
        }
        let tint = if selected { t.accent_text } else { t.icon };
        paint(ui, rect, name, (box_size * 0.55).round(), tint);
        if tooltip.is_empty() {
            resp
        } else {
            resp.on_hover_text(tooltip)
        }
    }
}

/// A white card surface with a hairline border and a soft drop shadow.
fn card_frame(t: &theme::Tokens) -> egui::Frame {
    egui::Frame::NONE
        .fill(t.card)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(12))
        .shadow(egui::Shadow {
            offset: [0, 1],
            blur: 6,
            spread: 0,
            color: Color32::from_black_alpha(if t.dark() { 40 } else { 16 }),
        })
}

/// A pill button with an optional leading icon; `primary` fills with the accent.
fn pill_button(ui: &mut Ui, icon: &str, label: &str, primary: bool) -> egui::Response {
    let t = theme::Tokens::get(ui.ctx());
    let font = theme::medium(13.0);
    let text_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(label.to_owned(), font.clone(), t.text)
            .size()
            .x
    });
    let w = text_w + if icon.is_empty() { 26.0 } else { 46.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w.max(58.0), 30.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label));
    let (fill, stroke, text) = if primary {
        (
            if resp.hovered() { t.accent_text } else { t.accent },
            Stroke::NONE,
            Color32::WHITE,
        )
    } else {
        (
            if resp.hovered() { t.hover } else { t.card },
            Stroke::new(1.2, t.text_muted),
            t.text,
        )
    };
    ui.painter()
        .rect(rect, CornerRadius::same(15), fill, stroke, StrokeKind::Inside);
    if icon.is_empty() {
        ui.painter()
            .text(rect.center(), Align2::CENTER_CENTER, label, font, text);
    } else {
        icons::paint(
            ui,
            Rect::from_min_size(rect.min + vec2(12.0, 7.0), vec2(16.0, 16.0)),
            icon,
            15.0,
            if primary { Color32::WHITE } else { t.icon },
        );
        ui.painter().text(
            pos2(rect.left() + 34.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            text,
        );
    }
    resp
}

/// Icon + label, transparent until hovered.
fn ghost_button(ui: &mut Ui, icon: &str, label: &str) -> egui::Response {
    let t = theme::Tokens::get(ui.ctx());
    let font = theme::medium(13.0);
    let text_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(label.to_owned(), font.clone(), t.text)
            .size()
            .x
    });
    let w = text_w + if icon.is_empty() { 20.0 } else { 38.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label));
    if resp.hovered() {
        ui.painter().rect_filled(rect, t.radius, t.hover);
    }
    if icon.is_empty() {
        ui.painter()
            .text(rect.center(), Align2::CENTER_CENTER, label, font, t.text);
    } else {
        icons::paint(
            ui,
            Rect::from_min_size(rect.min + vec2(6.0, 5.0), vec2(18.0, 18.0)),
            icon,
            17.0,
            t.icon,
        );
        ui.painter().text(
            pos2(rect.left() + 30.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            t.text,
        );
    }
    resp
}

/// A compact inline button, sized to line up with 13px status text (the regular
/// `ghost_button` is taller than the status line and gets clipped).
fn inline_button(ui: &mut Ui, t: &theme::Tokens, label: &str) -> egui::Response {
    let font = theme::regular(12.0);
    let text_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(label.to_owned(), font.clone(), t.text)
            .size()
            .x
    });
    let (rect, resp) = ui.allocate_exact_size(vec2(text_w + 16.0, 18.0), Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(4), t.hover);
    }
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, label, font, t.text);
    resp
}

/// A small rounded metadata badge (the file name, the rules file name, …).
fn badge(ui: &mut Ui, t: &theme::Tokens, text: &str) -> egui::Response {
    let font = theme::regular(12.0);
    let text_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(text.to_owned(), font.clone(), t.text_muted)
            .size()
            .x
    });
    let (rect, resp) = ui.allocate_exact_size(vec2(text_w.min(260.0) + 18.0, 22.0), Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(11),
        t.hover,
        Stroke::new(1.0, t.border),
        StrokeKind::Inside,
    );
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: font,
            color: t.text_muted,
            ..Default::default()
        },
    );
    job.wrap.max_width = rect.width() - 16.0;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    ui.painter()
        .galley(pos2(rect.left() + 9.0, rect.center().y - galley.size().y / 2.0), galley, t.text_muted);
    resp
}


/// Chip background and border for the three states: indexed (success), matching
/// the attribute filter (accent), or plain.
fn chip_colors(t: &theme::Tokens, highlight: bool, indexed: bool) -> (Color32, Stroke) {
    if indexed {
        (t.success_soft, Stroke::new(1.5, t.success))
    } else if highlight {
        (t.accent_soft, Stroke::new(1.5, t.accent))
    } else {
        (Color32::TRANSPARENT, Stroke::new(1.0, t.border))
    }
}

/// Small section label used in the controls bar.
fn muted_label(ui: &mut Ui, t: &theme::Tokens, text: &str) {
    ui.label(RichText::new(text).font(theme::regular(13.0)).color(t.text_muted));
}

/// A small uppercase section heading, like the PrintCraft docked panels use.
fn section_label(ui: &mut Ui, t: &theme::Tokens, text: &str) {
    ui.add_space(6.0);
    ui.label(
        RichText::new(text.to_uppercase())
            .font(theme::semibold(10.5))
            .color(t.text_faint),
    );
    ui.add_space(2.0);
}

/// Parse a single-byte delimiter argument.
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

/// A background result delivered over the job channel.
enum Job {
    Scan(u64, Result<ScanResult, String>),
    Rules(u64, Result<EvaluatedRules, String>),
    RuleRows(u64, Result<RuleHits, String>),
    Index(usize, PathBuf, Result<ColumnIndex, String>),
}

/// A deferred interaction collected while the row list is being drawn. The
/// grid's scroll closure only borrows `&self`, so state changes are queued and
/// applied once the frame's layout is done.
enum Action {
    RowClicked(usize),
    ChipClicked {
        row: usize,
        column: usize,
        double: bool,
    },
    ToggleIndex(usize),
    Mute(usize),
    ToggleLock(usize),
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
    /// The row opened in the floating detail form.
    detail: Option<DetailState>,
    /// Attribute whose value was last copied, so the UI can confirm the copy.
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
    /// Short feedback message about profile actions.
    profile_status: Option<String>,

    // ---- egui / app plumbing -------------------------------------------------
    theme_kind: theme::ThemeKind,
    /// First frame: install fonts and the palette.
    styled: bool,
    /// Fonts registered via `set_fonts` take effect on the next frame; named
    /// families would panic before that.
    fonts_ready: bool,
    /// Ask the row scroll area to jump back to the top on the next frame.
    scroll_to_top: bool,
    tx: Sender<Job>,
    rx: Receiver<Job>,
}

impl Viewer {
    fn new(args: Args) -> Self {
        let delimiter = parse_delimiter(&args.delimiter).unwrap_or(b',');
        let (tx, rx) = std::sync::mpsc::channel();
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
            detail: None,
            copy_notice: None,
            show_rules: false,
            show_config: false,
            rules: RulesState::default(),
            show_attr_names: false,
            filter_focus: None,
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
            attribute_filter: String::new(),
            profiles: load_config().profiles,
            current_profile: None,
            naming_profile: false,
            new_profile_name: String::new(),
            profile_status: None,
            theme_kind: theme::ThemeKind::Light,
            styled: false,
            fonts_ready: false,
            scroll_to_top: false,
            tx,
            rx,
        };
        if let Some(path) = args.path {
            viewer.load_file(path);
        }
        viewer
    }

    /// Whether any background pass is in flight (drives repaint scheduling).
    fn busy(&self) -> bool {
        self.scanning || self.rules.evaluating || self.rules.collecting || self.indexing
    }

    /// Open a native file picker (blocking; called from a button press).
    fn pick_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV / TSV", &["csv", "tsv", "txt"])
            .pick_file()
        {
            self.load_file(path);
        }
    }

    /// Switch to a new file: reset the per-file state, read its headers and
    /// start scanning. Any in-flight scan is invalidated by the generation bump.
    fn load_file(&mut self, path: PathBuf) {
        self.generation += 1;
        self.scanning = false;
        self.scan_started = None;
        self.scan_duration = None;
        self.dirty = false;
        self.rows.clear();
        self.detail = None;
        self.copy_notice = None;
        // The evaluated report belonged to the previous file; drop it (but keep
        // the chosen rules file for a quick re-evaluation).
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
                self.start_scan();
            }
            Err(message) => {
                self.headers.clear();
                self.error = Some(message);
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
    fn start_scan(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        // A fresh search always leaves the rule-filtered grid and abandons any
        // rule-row collection (the result would otherwise overwrite the scan).
        self.rules.view_active = false;
        discard_rule_hits(self.rules.hits.take());
        self.abort_rule_collection();
        self.sync_rule_attrs();
        if self.scanning {
            self.dirty = true;
            return;
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

        let tx = self.tx.clone();
        if index_mode {
            std::thread::spawn(move || {
                let result = scan_indexed(path, delimiter, pattern, indexes, limit);
                let _ = tx.send(Job::Scan(generation, result));
            });
        } else {
            std::thread::spawn(move || {
                let result = scan(path, delimiter, pattern, case_sensitive, limit, visible, parallel);
                let _ = tx.send(Job::Scan(generation, result));
            });
        }
    }

    /// Kick off a background evaluation of the selected rules file against the
    /// open CSV. The report (with its sample ids) fills the rules panel.
    fn start_rules_evaluation(&mut self) {
        let Some(csv) = self.path.clone() else {
            self.rules.error = Some("open a CSV file first".into());
            return;
        };
        let Some(rules_path) = self.rules.path.clone() else {
            self.rules.error = Some("select a rules file first".into());
            return;
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
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Compile once; the plan is returned so single-rule collections
            // never reload the file or rebuild the plan.
            let result = (|| {
                let mut program = dsl::load_file(&rules_path)?;
                program.defaults.report_limit = 50;
                let plan = rules::compile(program, &headers)?;
                let report = run_plan(&plan, &csv, delimiter, None, usize::MAX)?;
                Ok(EvaluatedRules {
                    plan: Arc::new(plan),
                    report,
                })
            })();
            let _ = tx.send(Job::Rules(generation, result));
        });
        if restore_grid {
            self.start_scan();
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
    fn show_rule_rows(&mut self, rule: usize, filter: Option<RowOutcome>) {
        // Toggling the exact view off restores the scan results.
        if self.rules.view_active
            && self.rules.hits.as_ref().map(|hits| hits.rule) == Some(rule)
            && self.rules.hits_filter == filter
        {
            self.clear_rule_view();
            return;
        }
        // Showing rule rows takes over the grid, so abandon any running scan.
        self.cancel_scan();
        // Data already collected: just switch side / re-activate.
        if self.rules.hits.as_ref().map(|hits| hits.rule) == Some(rule) {
            self.abort_rule_collection();
            self.rules.hits_filter = filter;
            self.rules.view_active = true;
            self.apply_rule_view();
            return;
        }
        // A collection for this rule is already running: keep it and just let
        // the grid show the outcome the user picked last when it lands.
        if self.rules.collecting && self.rules.pending_rule == Some(rule) {
            self.rules.queued_rule = None;
            self.rules.hits_filter = filter;
            return;
        }
        // Another rule is still being collected: queue this request instead of
        // running a second whole-file pass in parallel.
        if self.rules.collecting {
            self.rules.queued_rule = Some((rule, filter));
            return;
        }
        let (Some(csv), Some(plan)) = (self.path.clone(), self.rules.plan.clone()) else {
            self.rules.error = Some("evaluate the rules before browsing their rows".into());
            return;
        };
        // Collect the rule's rows and their full CSV records in one background
        // pass that evaluates *only* this rule.
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
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = collect_rule_hits(plan, rule, csv, delimiter, limit);
            let _ = tx.send(Job::RuleRows(generation, result));
        });
    }

    /// Drop any in-flight or queued rule-row collection.
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
    fn clear_rule_view(&mut self) {
        self.rules.view_active = false;
        self.abort_rule_collection();
        self.sync_rule_attrs();
        self.start_scan();
    }

    /// Apply (or lift) the "rule attributes only" restriction over the grid's
    /// hidden-attribute set.
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
    fn apply_rule_view(&mut self) {
        let (mut rows, matching, total) = {
            let Some(hits) = &self.rules.hits else {
                return;
            };
            let filter = self.rules.hits_filter;
            let mut rows = Vec::new();
            for (hit, row) in hits.hits.iter().zip(hits.rows.iter()) {
                let keep = match filter {
                    None => true,
                    Some(outcome) => hit.outcome == outcome,
                };
                if keep {
                    // Share the collected record; only the pointer is cloned.
                    rows.push(Arc::clone(row));
                }
            }
            // The retained list is capped, so the exact totals come from the
            // full evaluation rather than from the collected rows.
            (
                rows,
                hits.total_for(filter) as usize,
                hits.total_for(None) as usize,
            )
        };
        // Honor the "rows" drop-down even in the rule view: the filter stays in
        // force, only the number of displayed rows changes. `matched` keeps the
        // full count so the status line can say how many were truncated.
        rows.truncate(self.limit);
        self.rows = rows;
        self.matched = matching;
        self.truncated = matching > self.rows.len();
        self.rows_read = total;
        self.indexed_result = false;
        self.error = None;
        self.scroll_to_top = true;
        self.sync_rule_attrs();
    }

    /// The header of a column, or `?` when the index is out of range.
    fn header(&self, column: usize) -> &str {
        self.headers.get(column).map(String::as_str).unwrap_or("?")
    }

    /// Whether the search is currently served by the built indexes.
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
    fn rescan_if_searching_visible(&mut self) {
        if self.visible_only {
            self.start_scan();
        }
    }

    /// Snapshot one matching row into the detail form.
    fn open_detail(&mut self, row: usize) {
        let fields = self.rows.get(row).map(|values| {
            (0..values.len())
                .map(|column| (self.header(column).to_string(), values[column].clone()))
                .collect::<Vec<_>>()
        });
        if let Some(fields) = fields {
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

    // ---- direct actions (formerly `Message` variants) ------------------------

    fn on_filter_changed(&mut self) {
        self.filter_focus = Some(FilterFocus::Results);
        self.last_edit = Some(Instant::now());
        self.debounce_pending = true;
    }

    fn run_filter(&mut self) {
        self.debounce_pending = false;
        self.start_scan();
    }

    fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
        // The rule view is a filter, not a scan: changing how many rows are
        // shown must keep the rule / outcome filter in place.
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
                    self.show_rule_rows(rule, filter);
                    return;
                }
            }
            self.apply_rule_view();
            return;
        }
        self.start_scan();
    }

    fn toggle_index(&mut self, column: usize) {
        if self.indexes.remove(&column).is_some() {
            self.index_status = Some(format!("dropped index on “{}”", self.header(column)));
            // The search falls back to the regex now.
            self.start_scan();
        } else if self.indexing {
            self.index_status = Some("an index build is already running".into());
        } else if let Some(path) = self.path.clone() {
            self.indexing = true;
            self.index_status = Some(format!("indexing “{}”…", self.header(column)));
            let delimiter = self.delimiter;
            let built_path = path.clone();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result = build_index(path, delimiter, column);
                let _ = tx.send(Job::Index(column, built_path, result));
            });
        }
    }

    fn mute(&mut self, index: usize) {
        if self.locked.contains(&index) {
            return;
        }
        self.muted.insert(index);
        self.profile_status = None;
        self.rescan_if_searching_visible();
    }

    fn unmute(&mut self, index: usize) {
        self.muted.remove(&index);
        self.profile_status = None;
        self.rescan_if_searching_visible();
    }

    fn unmute_all(&mut self) {
        self.muted.clear();
        self.profile_status = None;
        self.rescan_if_searching_visible();
    }

    fn mute_all(&mut self) {
        self.muted = (0..self.headers.len())
            .filter(|column| !self.locked.contains(column))
            .collect();
        self.profile_status = None;
        self.rescan_if_searching_visible();
    }

    fn toggle_lock(&mut self, index: usize) {
        if !self.locked.remove(&index) {
            self.locked.insert(index);
            self.muted.remove(&index);
        }
        self.profile_status = None;
        self.sync_rule_attrs();
        self.rescan_if_searching_visible();
    }

    fn toggle_rule_attrs_only(&mut self, checked: bool) {
        self.rules.attrs_only = checked;
        self.sync_rule_attrs();
    }

    fn copy_value(&mut self, attribute: &str, value: &str, ctx: &egui::Context) {
        self.copy_notice = Some(attribute.to_string());
        ctx.copy_text(value.to_string());
    }

    /// Escape: close the row form, or clear the filter box being edited.
    fn escape(&mut self) {
        if self.detail.is_some() {
            self.detail = None;
            self.copy_notice = None;
            return;
        }
        let attributes_first = self.filter_focus == Some(FilterFocus::Attributes);
        if attributes_first && !self.attribute_filter.is_empty() {
            self.attribute_filter.clear();
        } else if !attributes_first && !self.filter.is_empty() {
            self.filter.clear();
            self.debounce_pending = false;
            self.start_scan();
        } else if !self.attribute_filter.is_empty() {
            self.attribute_filter.clear();
        } else if !self.filter.is_empty() {
            self.filter.clear();
            self.debounce_pending = false;
            self.start_scan();
        }
    }

    /// Drain finished background work.
    fn handle_jobs(&mut self) {
        while let Ok(job) = self.rx.try_recv() {
            match job {
                Job::Scan(generation, result) => self.on_scan_finished(generation, result),
                Job::Rules(generation, result) => self.on_rules_evaluated(generation, result),
                Job::RuleRows(generation, result) => {
                    self.on_rule_rows_collected(generation, result);
                }
                Job::Index(column, path, result) => self.on_index_built(column, path, result),
            }
        }
    }

    fn on_scan_finished(&mut self, generation: u64, result: Result<ScanResult, String>) {
        if generation != self.generation {
            return;
        }
        match result {
            Ok(scan) => {
                self.rows = scan.rows.into_iter().map(Arc::new).collect();
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
        self.scroll_to_top = true;
        // Re-scan when a mute change or a filter edit landed while the scan was
        // running.
        let stale = self.last_scanned.as_deref() != Some(self.filter.as_str());
        if self.dirty || stale {
            self.dirty = false;
            self.start_scan();
        }
    }

    fn on_rules_evaluated(&mut self, generation: u64, result: Result<EvaluatedRules, String>) {
        if generation != self.rules.generation {
            return;
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
    }

    fn on_rule_rows_collected(&mut self, generation: u64, result: Result<RuleHits, String>) {
        if generation != self.rules.generation {
            return;
        }
        self.rules.collecting = false;
        self.rules.pending_rule = None;
        self.rules.collect_duration = self.rules.collect_started.take().map(|start| start.elapsed());
        match result {
            Ok(hits) => {
                // Only one rule's rows are held at a time; the next collection
                // drops these before it runs.
                self.rules.hits = Some(hits);
                self.rules.view_active = true;
                self.apply_rule_view();
            }
            Err(message) => {
                discard_rule_hits(self.rules.hits.take());
                self.rules.view_active = false;
                self.rules.error = Some(message);
            }
        }
        // A rule the user asked for while this pass was running is evaluated
        // now, so clicks are never dropped.
        if let Some((queued, filter)) = self.rules.queued_rule.take() {
            self.show_rule_rows(queued, filter);
        }
    }

    fn on_index_built(&mut self, column: usize, path: PathBuf, result: Result<ColumnIndex, String>) {
        // The file may have been switched while the index was building; the
        // offsets would be meaningless, so drop the result.
        if self.path.as_deref() != Some(path.as_path()) {
            return;
        }
        self.indexing = false;
        match result {
            Ok(index) => {
                let count = index.entries.len();
                let name = self.header(column).to_string();
                self.indexes.insert(column, Arc::new(index));
                self.index_status = Some(format!("indexed “{name}” ({count} rows)"));
                // Re-run the search: the index now serves a prefix query.
                self.start_scan();
            }
            Err(message) => {
                self.index_status = Some(message);
            }
        }
    }

    fn apply_action(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::RowClicked(row) => self.open_detail(row),
            Action::ChipClicked {
                row,
                column,
                double,
            } => {
                let Some(value) = self
                    .rows
                    .get(row)
                    .and_then(|values| values.get(column))
                    .cloned()
                else {
                    return;
                };
                self.copy_notice = Some(self.header(column).to_string());
                ctx.copy_text(value);
                if double {
                    self.open_detail(row);
                }
            }
            Action::ToggleIndex(column) => self.toggle_index(column),
            Action::Mute(column) => self.mute(column),
            Action::ToggleLock(column) => self.toggle_lock(column),
        }
    }
}

/// Rule-panel interaction, queued while the panel is drawn.
enum RuleAction {
    All(usize),
    Filter(usize, RowOutcome),
    Clear,
    ToggleAttrsOnly(bool),
    Close,
    OpenRules,
}

impl Viewer {
    fn welcome(&mut self, ui: &mut Ui) {
        let t = theme::Tokens::get(ui.ctx());
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            ui.add_space((ui.available_height() * 0.5 - 140.0).max(30.0));
            card_frame(&t)
                .inner_margin(Margin::symmetric(40, 36))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        let (rect, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
                        ui.painter().rect(
                            rect,
                            CornerRadius::same(14),
                            t.accent_soft,
                            Stroke::NONE,
                            StrokeKind::Inside,
                        );
                        icons::paint(ui, rect, "file-spreadsheet", 30.0, t.accent_text);
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new("No CSV file opened").font(theme::semibold(22.0)),
                        );
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new("Grep and browse rows of a large CSV file.")
                                .font(theme::regular(14.0))
                                .color(t.text_muted),
                        );
                        ui.add_space(18.0);
                        if pill_button(ui, "folder-open", "Open CSV…", true).clicked() {
                            self.pick_file();
                        }
                    });
                });
        });
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let t = theme::Tokens::get(ui.ctx());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let (mark, _) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::hover());
            ui.painter().rect(
                mark,
                CornerRadius::same(6),
                t.accent,
                Stroke::NONE,
                StrokeKind::Inside,
            );
            icons::paint(ui, mark, "file-spreadsheet", 15.0, Color32::WHITE);
            ui.label(RichText::new("fview").font(theme::semibold(15.0)));
            if pill_button(ui, "folder-open", "Open…", false).clicked() {
                self.pick_file();
            }
            if pill_button(ui, "list-checks", "Rules", false).clicked() {
                self.show_rules = !self.show_rules;
            }
            if pill_button(ui, "settings-2", "Config", false).clicked() {
                self.show_config = !self.show_config;
            }
            if let Some(path) = self.path.clone() {
                if let Some(name) = path.file_name() {
                    let _ = badge(ui, &t, &name.to_string_lossy())
                        .on_hover_text(path.display().to_string());
                }
            }
            muted_label(ui, &t, "Filter");
            // Reserve room for the Search button and the theme toggle.
            let reserve = 116.0 + 36.0;
            let w = (ui.available_width() - reserve).max(120.0);
            let hint = if self.index_mode() {
                "beginsWith prefix over indexed columns, e.g. Fra"
            } else {
                "regex filter, e.g. \\d{4}-\\d{2}"
            };
            let resp = ui.add_sized(
                [w, 30.0],
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(hint)
                    .font(theme::regular(14.0))
                    .vertical_align(Align::Center)
                    .margin(Margin::symmetric(8, 4)),
            );
            if resp.changed() {
                self.on_filter_changed();
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.run_filter();
            }
            if pill_button(ui, "search", "Search", true).clicked() {
                self.run_filter();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (icon, tip) = if t.dark() {
                    ("sun", "Light theme")
                } else {
                    ("moon", "Dark theme")
                };
                if icons::button(ui, icon, 26.0, false, tip).clicked() {
                    self.theme_kind = if t.dark() {
                        theme::ThemeKind::Light
                    } else {
                        theme::ThemeKind::Dark
                    };
                    theme::apply(ui.ctx(), self.theme_kind);
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let t = theme::Tokens::get(ui.ctx());
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if let Some(error) = &self.error {
                let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                icons::paint(ui, r, "triangle-alert", 13.0, t.danger);
                ui.label(
                    RichText::new(error.as_str())
                        .font(theme::regular(13.0))
                        .color(t.danger),
                );
            } else if let Some(active) = self.active_rule() {
                let label = match self.rules.hits_filter {
                    None => "all rows",
                    Some(RowOutcome::Passed) => "matching rows",
                    Some(RowOutcome::Failed) => "failing rows",
                    Some(RowOutcome::Skipped) => "skipped rows",
                    Some(RowOutcome::ValidationSkipped) => "validation-skipped rows",
                };
                let elapsed = self
                    .rules
                    .collect_duration
                    .map(|d| format!(" · {}", format_duration(d)))
                    .unwrap_or_default();
                let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                icons::paint(ui, r, "list-checks", 13.0, t.accent);
                ui.label(
                    RichText::new(format!(
                        "rule {} · {label} ({}){elapsed}",
                        active + 1,
                        self.rows.len()
                    ))
                    .font(theme::regular(13.0))
                    .color(t.accent_text),
                );
                if inline_button(ui, &t, "clear").clicked() {
                    self.clear_rule_view();
                }
            } else if self.scanning {
                let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                icons::paint(ui, r, "clock-3", 13.0, t.accent);
                ui.label(
                    RichText::new("scanning…")
                        .font(theme::regular(13.0))
                        .color(t.accent_text),
                );
            } else {
                let elapsed = self.scan_duration;
                ui.label(
                    RichText::new(status_text(
                        self.rows.len(),
                        self.matched,
                        self.rows_read,
                        self.truncated,
                        self.indexed_result,
                        elapsed,
                    ))
                    .font(theme::regular(13.0))
                    .color(t.text_muted),
                );
            }
            // Transient feedback (copies, index builds, profile saves) sits on
            // the right of the status line rather than crowding the controls.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(attribute) = &self.copy_notice {
                    let (r, _) = ui.allocate_exact_size(vec2(13.0, 13.0), Sense::hover());
                    icons::paint(ui, r, "check", 12.0, t.success);
                    ui.label(
                        RichText::new(format!("copied {attribute}"))
                            .font(theme::regular(12.0))
                            .color(t.success),
                    );
                }
                if let Some(status) = &self.index_status {
                    ui.label(
                        RichText::new(status.as_str())
                            .font(theme::regular(12.0))
                            .color(t.text_muted),
                    );
                }
                if let Some(status) = &self.profile_status {
                    ui.label(
                        RichText::new(status.as_str())
                            .font(theme::regular(12.0))
                            .color(t.text_muted),
                    );
                }
            });
        });
    }

    /// The configuration panel: scan/view options, attribute visibility and
    /// saved profiles, docked like the Rules panel.
    fn config_panel(&mut self, ui: &mut Ui) {
        let t = theme::Tokens::get(ui.ctx());
        let width = sidebar_width(ui.ctx().content_rect().width());
        // Width available inside the 12px frame margins and the floating scrollbar.
        let available = (width - 34.0).max(80.0);
        let this = &mut *self;
        egui::Panel::left("fview-config")
            .resizable(false)
            .exact_size(width)
            .frame(
                egui::Frame::NONE
                    .fill(t.panel)
                    .stroke(Stroke::new(1.0, t.divider))
                    .inner_margin(Margin::same(12)),
            )
            .show(ui, |ui| this.config_panel_body(ui, &t, available));
    }

    fn config_panel_body(&mut self, ui: &mut Ui, t: &theme::Tokens, available: f32) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
            icons::paint(ui, r, "settings-2", 15.0, t.accent);
            ui.label(RichText::new("Config").font(theme::semibold(16.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icons::button(ui, "x", 24.0, false, "Close panel").clicked() {
                    self.show_config = false;
                }
            });
        });
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt("fview-config")
            .show(ui, |ui| {
                // --- View ---------------------------------------------------
                section_label(ui, t, "View");
                if ui.checkbox(&mut self.visible_only, "visible only").changed() {
                    self.start_scan();
                }
                if ui.checkbox(&mut self.parallel, "parallel").changed() {
                    self.start_scan();
                }
                let _ = ui.checkbox(&mut self.table, "table");
                let can_index = !self.indexes.is_empty();
                ui.add_enabled_ui(can_index, |ui| {
                    if ui.checkbox(&mut self.use_index, "index").changed() {
                        self.start_scan();
                    }
                });
                let _ = ui.checkbox(&mut self.show_attr_names, "attribute names");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    muted_label(ui, t, "Rows");
                    let mut limit_choices = ROW_LIMIT_CHOICES.to_vec();
                    if !limit_choices.contains(&self.limit) {
                        limit_choices.push(self.limit);
                        limit_choices.sort_unstable();
                    }
                    let mut selected = self.limit;
                    egui::ComboBox::from_id_salt("fview-rows")
                        .selected_text(self.limit.to_string())
                        .width(100.0)
                        .show_ui(ui, |ui| {
                            for choice in &limit_choices {
                                ui.selectable_value(&mut selected, *choice, choice.to_string());
                            }
                        });
                    if selected != self.limit {
                        self.set_limit(selected);
                    }
                });

                // --- Attributes ---------------------------------------------
                section_label(ui, t, "Attributes");
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(8.0, 4.0);
                    if ghost_button(ui, "eye-off", "Hide all").clicked() {
                        self.mute_all();
                    }
                    if !self.muted.is_empty() {
                        let caret = if self.show_hidden {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        };
                        if ghost_button(ui, caret, &format!("Hidden ({})", self.muted.len()))
                            .clicked()
                        {
                            self.show_hidden = !self.show_hidden;
                        }
                        if ghost_button(ui, "", "show all").clicked() {
                            self.unmute_all();
                        }
                    }
                });
                ui.add_space(4.0);
                let resp = ui.add_sized(
                    [available, 26.0],
                    egui::TextEdit::singleline(&mut self.attribute_filter)
                        .hint_text("filter attributes…")
                        .font(theme::regular(13.0))
                        .vertical_align(Align::Center)
                        .margin(Margin::symmetric(6, 3)),
                );
                if resp.changed() {
                    self.filter_focus = Some(FilterFocus::Attributes);
                }
                if !self.muted.is_empty() {
                    if !self.show_hidden {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(hidden_note(self.muted.len()))
                                .font(theme::regular(12.0))
                                .color(t.text_muted),
                        );
                    } else {
                        let mut indices: Vec<usize> = self.muted.iter().copied().collect();
                        indices.sort_by(|a, b| {
                            let left = self.headers.get(*a).map(String::as_str).unwrap_or("");
                            let right = self.headers.get(*b).map(String::as_str).unwrap_or("");
                            left.to_lowercase()
                                .cmp(&right.to_lowercase())
                                .then_with(|| a.cmp(b))
                        });
                        let searching = !self.attribute_filter.trim().is_empty();
                        let items: Vec<(usize, String)> = indices
                            .into_iter()
                            .filter(|index| {
                                !searching
                                    || attr_matches(&self.attribute_filter, self.header(*index))
                            })
                            .map(|index| (index, self.header(index).to_string()))
                            .collect();
                        ui.add_space(6.0);
                        if items.is_empty() {
                            ui.label(
                                RichText::new("no hidden attributes match the filter")
                                    .font(theme::regular(12.0))
                                    .color(t.text_muted),
                            );
                        } else {
                            let widths: Vec<f32> = items
                                .iter()
                                .map(|(_, name)| name.chars().count() as f32 * CHAR_WIDTH + 72.0)
                                .collect();
                            for range in chip_lines(&widths, available) {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    for position in range {
                                        let (index, name) = &items[position];
                                        let index = *index;
                                        let indexed = self.indexes.contains_key(&index);
                                        let (fill, stroke) = chip_colors(t, false, indexed);
                                        egui::Frame::NONE
                                            .fill(fill)
                                            .stroke(stroke)
                                            .corner_radius(CornerRadius::same(RADIUS))
                                            .inner_margin(Margin::symmetric(8, 2))
                                            .show(ui, |ui| {
                                                ui.spacing_mut().item_spacing.x = 6.0;
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        RichText::new(name)
                                                            .font(theme::regular(13.0))
                                                            .color(t.text),
                                                    );
                                                    if icons::button(
                                                        ui,
                                                        "columns-3",
                                                        18.0,
                                                        indexed,
                                                        "Prefix index",
                                                    )
                                                    .clicked()
                                                    {
                                                        self.toggle_index(index);
                                                    }
                                                    if icons::button(
                                                        ui,
                                                        "eye",
                                                        18.0,
                                                        false,
                                                        "Show attribute",
                                                    )
                                                    .clicked()
                                                    {
                                                        self.unmute(index);
                                                    }
                                                });
                                            });
                                    }
                                });
                                ui.add_space(6.0);
                            }
                        }
                    }
                }

                // --- Profiles -----------------------------------------------
                section_label(ui, t, "Profiles");
                let names: Vec<String> = self.profiles.keys().cloned().collect();
                let mut chosen = self.current_profile.clone();
                egui::ComboBox::from_id_salt("fview-profile")
                    .selected_text(chosen.clone().unwrap_or_else(|| "none".into()))
                    .width(available)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(chosen.is_none(), "none").clicked() {
                            chosen = None;
                        }
                        for name in &names {
                            if ui
                                .selectable_label(chosen.as_deref() == Some(name.as_str()), name)
                                .clicked()
                            {
                                chosen = Some(name.clone());
                            }
                        }
                    });
                if chosen != self.current_profile {
                    match &chosen {
                        Some(name) => self.apply_profile(name),
                        None => {
                            self.current_profile = None;
                            self.profile_status = None;
                        }
                    }
                }
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(8.0, 4.0);
                    if self.current_profile.is_some() {
                        if ui.button("Save").clicked() {
                            self.save_current_profile();
                        }
                        if ghost_button(ui, "", "clear").clicked() {
                            self.current_profile = None;
                            self.profile_status = None;
                        }
                    }
                    if ui.button("Save as new…").clicked() {
                        self.naming_profile = true;
                        self.new_profile_name.clear();
                        self.profile_status = None;
                    }
                });
                if self.naming_profile {
                    ui.add_space(4.0);
                    let resp = ui.add_sized(
                        [available, 26.0],
                        egui::TextEdit::singleline(&mut self.new_profile_name)
                            .hint_text("profile name")
                            .vertical_align(Align::Center)
                            .margin(Margin::symmetric(6, 3)),
                    );
                    let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() || submit {
                            self.save_new_profile();
                        }
                        if ghost_button(ui, "", "Cancel").clicked() {
                            self.naming_profile = false;
                            self.new_profile_name.clear();
                            self.profile_status = None;
                        }
                    });
                }

                // --- Appearance ---------------------------------------------
                section_label(ui, t, "Appearance");
                let (icon, label) = if t.dark() {
                    ("sun", "Light theme")
                } else {
                    ("moon", "Dark theme")
                };
                if ghost_button(ui, icon, label).clicked() {
                    self.theme_kind = if t.dark() {
                        theme::ThemeKind::Light
                    } else {
                        theme::ThemeKind::Dark
                    };
                    theme::apply(ui.ctx(), self.theme_kind);
                }
            });
    }

    fn grid(&mut self, ui: &mut Ui) {
        let t = theme::Tokens::get(ui.ctx());
        let all_hidden = !self.headers.is_empty() && self.muted.len() >= self.headers.len();
        let show_table = self.table && !all_hidden && !self.headers.is_empty();
        let scroll_to_top = self.scroll_to_top;
        self.scroll_to_top = false;

        let mut actions: Vec<Action> = Vec::new();
        let mut scroll = if show_table {
            egui::ScrollArea::both()
        } else {
            egui::ScrollArea::vertical()
        };
        if scroll_to_top {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        let scroll = scroll.auto_shrink([false, false]).id_salt("fview-rows");
        let this = &*self;
        card_frame(&t).inner_margin(Margin::same(2)).show(ui, |ui| {
            scroll.show_viewport(ui, |ui, viewport| {
                this.draw_rows(ui, viewport, &mut actions);
            });
        });
        for action in actions {
            self.apply_action(action, ui.ctx());
        }
    }

    fn draw_rows(&self, ui: &mut Ui, viewport: Rect, actions: &mut Vec<Action>) {
        let t = theme::Tokens::get(ui.ctx());
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);

        if self.rows.is_empty() {
            let message = if self.scanning {
                "scanning…"
            } else {
                "no rows match the filter"
            };
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(STRIPE_PADDING_H);
                ui.label(
                    RichText::new(message)
                        .font(theme::regular(14.0))
                        .color(t.text_muted),
                );
            });
            return;
        }
        let all_hidden = !self.headers.is_empty() && self.muted.len() >= self.headers.len();
        if all_hidden {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(STRIPE_PADDING_H);
                ui.label(
                    RichText::new(
                        "All attributes hidden — reveal the Hidden list above, then click an attribute to display it.",
                    )
                    .font(theme::regular(14.0))
                    .color(t.text_muted),
                );
            });
            return;
        }

        let show_table = self.table && !self.headers.is_empty();
        let visible_columns: Vec<usize> = (0..self.headers.len())
            .filter(|index| !self.muted.contains(index))
            .collect();
        let available = chip_area_width(viewport.width());
        let show_names = self.show_attr_names;

        let column_widths: Vec<f32> = if show_table {
            visible_columns
                .iter()
                .map(|&column| {
                    (self.header(column).chars().count() as f32 * CHAR_WIDTH + 24.0)
                        .max(TABLE_CELL_MIN_WIDTH)
                })
                .collect()
        } else {
            Vec::new()
        };
        let table_width: f32 = if show_table {
            column_widths.iter().sum::<f32>().max(1.0)
        } else {
            0.0
        };

        let total_rows = self.rows.len();
        let row_heights: Vec<f32> = if show_table {
            vec![TABLE_ROW_HEIGHT; total_rows]
        } else {
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
        let mut row_tops = Vec::with_capacity(total_rows + 1);
        row_tops.push(0.0f32);
        for height in &row_heights {
            let last = row_tops[row_tops.len() - 1];
            row_tops.push(last + height);
        }
        let total_height = row_tops[total_rows];

        let header_h = if show_table { TABLE_ROW_HEIGHT } else { 0.0 };
        if show_table {
            self.draw_table_header(ui, &visible_columns, &column_widths, table_width, actions);
        }

        let rel_top = (viewport.min.y - header_h).max(0.0);
        let rel_bottom = (viewport.max.y - header_h).max(0.0);
        let first = row_tops
            .partition_point(|&top| top <= rel_top)
            .saturating_sub(1 + OVERSCAN_ROWS)
            .min(total_rows);
        let last = (row_tops
            .partition_point(|&top| top < rel_bottom)
            + OVERSCAN_ROWS
            + 1)
            .min(total_rows)
            .max(first);

        if first > 0 {
            ui.add_space(row_tops[first]);
        }
        for index in first..last {
            let height = row_heights[index];
            if show_table {
                self.draw_table_row(
                    ui,
                    index,
                    height,
                    &visible_columns,
                    &column_widths,
                    table_width,
                    actions,
                );
            } else {
                self.draw_chip_stripe(
                    ui,
                    index,
                    height,
                    &visible_columns,
                    available,
                    show_names,
                    actions,
                );
            }
        }
        if last < total_rows {
            ui.add_space(total_height - row_tops[last]);
        }
    }

    fn draw_table_header(
        &self,
        ui: &mut Ui,
        columns: &[usize],
        widths: &[f32],
        table_width: f32,
        actions: &mut Vec<Action>,
    ) {
        let t = theme::Tokens::get(ui.ctx());
        let (rect, _) = ui.allocate_exact_size(vec2(table_width, TABLE_ROW_HEIGHT), Sense::hover());
        let mut x = rect.left();
        for (position, &column) in columns.iter().enumerate() {
            let w = widths[position];
            let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(w, TABLE_ROW_HEIGHT));
            let indexed = self.indexes.contains_key(&column);
            let locked = self.locked.contains(&column);
            ui.painter().rect_filled(
                cell,
                0.0,
                if indexed { t.success_soft } else { t.hover },
            );
            let mut cui = ui.new_child(
                UiBuilder::new()
                    .max_rect(cell.shrink2(vec2(8.0, 0.0)))
                    .layout(Layout::left_to_right(Align::Center)),
            );
            cui.shrink_clip_rect(cell);
            let label_w = (w - 78.0).max(16.0);
            cui.add_sized(
                [label_w, TABLE_ROW_HEIGHT],
                egui::Label::new(
                    RichText::new(self.header(column))
                        .font(theme::medium(13.0))
                        .color(t.text),
                )
                .truncate(),
            );
            if icons::button(&mut cui, "columns-3", 18.0, indexed, "Prefix index").clicked() {
                actions.push(Action::ToggleIndex(column));
            }
            if !locked
                && icons::button(&mut cui, "eye-off", 18.0, false, "Hide attribute").clicked()
            {
                actions.push(Action::Mute(column));
            }
            let lock_icon = if locked { "lock" } else { "lock-open" };
            if icons::button(&mut cui, lock_icon, 18.0, locked, "Pin attribute").clicked() {
                actions.push(Action::ToggleLock(column));
            }
            ui.painter()
                .vline(cell.right(), cell.y_range(), Stroke::new(1.0, t.divider));
            x += w;
        }
        ui.painter()
            .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, t.border));
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_table_row(
        &self,
        ui: &mut Ui,
        index: usize,
        height: f32,
        columns: &[usize],
        widths: &[f32],
        table_width: f32,
        actions: &mut Vec<Action>,
    ) {
        let t = theme::Tokens::get(ui.ctx());
        let striped = index % 2 == 1;
        let (rect, resp) = ui.allocate_exact_size(vec2(table_width, height), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, 0.0, t.accent_soft);
        } else if striped {
            ui.painter().rect_filled(rect, 0.0, t.hover);
        }
        if resp.clicked() {
            actions.push(Action::RowClicked(index));
        }
        let values = &self.rows[index];
        let mut x = rect.left();
        for (position, &column) in columns.iter().enumerate() {
            let w = widths[position];
            let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(w, height));
            let value = values.get(column).map(String::as_str).unwrap_or("");
            let mut cui = ui.new_child(
                UiBuilder::new()
                    .max_rect(cell.shrink2(vec2(10.0, 0.0)))
                    .layout(Layout::left_to_right(Align::Center)),
            );
            cui.shrink_clip_rect(cell);
            cui.add_sized(
                [(w - 20.0).max(10.0), height],
                egui::Label::new(
                    RichText::new(value)
                        .font(theme::regular(13.0))
                        .color(t.text),
                )
                .truncate(),
            );
            ui.painter()
                .vline(cell.right(), cell.y_range(), Stroke::new(1.0, t.divider));
            x += w;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_chip_stripe(
        &self,
        ui: &mut Ui,
        index: usize,
        height: f32,
        columns: &[usize],
        available: f32,
        show_names: bool,
        actions: &mut Vec<Action>,
    ) {
        let t = theme::Tokens::get(ui.ctx());
        let striped = index % 2 == 1;
        let width = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());
        if resp.hovered() {
            ui.painter()
                .rect_filled(rect, 0.0, t.accent_soft.gamma_multiply(0.5));
        } else if striped {
            ui.painter().rect_filled(rect, 0.0, t.hover);
        }
        if resp.clicked() {
            actions.push(Action::RowClicked(index));
        }

        let values = &self.rows[index];
        let widths: Vec<f32> = columns
            .iter()
            .map(|&column| {
                chip_estimate(
                    self.header(column),
                    values.get(column).map(String::as_str).unwrap_or(""),
                    show_names,
                )
            })
            .collect();
        let lines = chip_lines(&widths, available);
        let inner = Rect::from_min_max(
            rect.min + vec2(STRIPE_PADDING_H, STRIPE_PADDING),
            rect.max - vec2(STRIPE_PADDING_H, STRIPE_PADDING),
        );
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(inner)
                .layout(Layout::top_down(Align::Min)),
        );
        child.shrink_clip_rect(rect);
        child.spacing_mut().item_spacing.y = CHIP_LINE_SPACING;
        for range in lines {
            child.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = CHIP_SPACING;
                for position in range {
                    let column = columns[position];
                    let header = self.header(column);
                    let value = values.get(column).map(String::as_str).unwrap_or("");
                    let highlight = attr_matches(&self.attribute_filter, header);
                    let indexed = self.indexes.contains_key(&column);
                    let locked = self.locked.contains(&column);
                    self.draw_chip(
                        ui,
                        index,
                        column,
                        header,
                        value,
                        available,
                        highlight,
                        indexed,
                        locked,
                        show_names,
                        actions,
                    );
                }
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_chip(
        &self,
        ui: &mut Ui,
        row: usize,
        column: usize,
        header: &str,
        value: &str,
        max_width: f32,
        highlight: bool,
        indexed: bool,
        locked: bool,
        show_name: bool,
        actions: &mut Vec<Action>,
    ) {
        let t = theme::Tokens::get(ui.ctx());
        let full_label = format!("{header} = {value}");
        let label = if show_name {
            full_label.clone()
        } else {
            value.to_string()
        };
        let chip_w = chip_estimate(header, value, show_name)
            .min(max_width)
            .max(60.0);
        let label_w = (chip_w - CHIP_CHROME).max(24.0);
        let (fill, stroke) = chip_colors(&t, highlight, indexed);
        egui::Frame::NONE
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(RADIUS))
            .inner_margin(Margin::symmetric(6, 2))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.horizontal(|ui| {
                    let r = ui.add_sized(
                        [label_w, CHIP_LINE_BOX - 8.0],
                        egui::Label::new(
                            RichText::new(label)
                                .font(theme::regular(13.0))
                                .color(t.text),
                        )
                        .truncate()
                        .sense(Sense::click()),
                    );
                    if r.double_clicked() {
                        actions.push(Action::ChipClicked {
                            row,
                            column,
                            double: true,
                        });
                    } else if r.clicked() {
                        actions.push(Action::ChipClicked {
                            row,
                            column,
                            double: false,
                        });
                    }
                    let _ = r.on_hover_text(full_label);
                    if icons::button(ui, "columns-3", 18.0, indexed, "Prefix index").clicked() {
                        actions.push(Action::ToggleIndex(column));
                    }
                    if !locked
                        && icons::button(ui, "eye-off", 18.0, false, "Hide attribute").clicked()
                    {
                        actions.push(Action::Mute(column));
                    }
                    let lock_icon = if locked { "lock" } else { "lock-open" };
                    if icons::button(ui, lock_icon, 18.0, locked, "Pin attribute").clicked() {
                        actions.push(Action::ToggleLock(column));
                    }
                });
            });
    }

    fn rules_panel(&mut self, ui: &mut Ui) {
        let t = theme::Tokens::get(ui.ctx());
        let mut actions: Vec<RuleAction> = Vec::new();
        let width = sidebar_width(ui.ctx().content_rect().width());
        {
            let this = &*self;
            egui::Panel::right("fview-rules")
                .resizable(false)
                .exact_size(width)
                .frame(
                    egui::Frame::NONE
                        .fill(t.panel)
                        .stroke(Stroke::new(1.0, t.divider))
                        .inner_margin(Margin::same(12)),
                )
                .show(ui, |ui| this.rules_panel_body(ui, &t, &mut actions));
        }
        for action in actions {
            self.apply_rule_action(action);
        }
    }

    fn rules_panel_body(&self, ui: &mut Ui, t: &theme::Tokens, actions: &mut Vec<RuleAction>) {
        let rules_name = self
            .rules
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "no rules file".to_string());

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
            icons::paint(ui, r, "list-checks", 15.0, t.accent);
            ui.label(RichText::new("Rules").font(theme::semibold(16.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icons::button(ui, "x", 24.0, false, "Close panel").clicked() {
                    actions.push(RuleAction::Close);
                }
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if pill_button(ui, "folder-open", "Open rules…", false).clicked() {
                actions.push(RuleAction::OpenRules);
            }
            let _ = badge(ui, t, &rules_name);
        });
        ui.add_space(2.0);
        let mut attrs_only = self.rules.attrs_only;
        if ui
            .checkbox(&mut attrs_only, "rule attributes only")
            .changed()
        {
            actions.push(RuleAction::ToggleAttrsOnly(attrs_only));
        }

        if self.rules.evaluating {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                icons::paint(ui, r, "clock-3", 13.0, t.accent);
                ui.label(
                    RichText::new("evaluating rules…")
                        .font(theme::regular(13.0))
                        .color(t.accent_text),
                );
            });
        }
        if let Some(error) = &self.rules.error {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                icons::paint(ui, r, "triangle-alert", 13.0, t.danger);
                ui.label(
                    RichText::new(error.as_str())
                        .font(theme::regular(13.0))
                        .color(t.danger),
                );
            });
        }

        section_label(ui, t, "Rules");
        if let Some(report) = &self.rules.report {
            ui.label(
                RichText::new(format!(
                    "{} rules · {} passed · {} failed",
                    report.rules_total, report.rules_passed, report.rules_failed
                ))
                .font(theme::regular(12.0))
                .color(t.text_muted),
            );
            ui.add_space(4.0);
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt("fview-rules-list")
            .show(ui, |ui| {
                if let Some(report) = &self.rules.report {
                    for (index, rule) in report.rules.iter().enumerate() {
                        self.rule_card(ui, t, index, rule, actions);
                        ui.add_space(4.0);
                    }
                } else if !self.rules.evaluating && self.rules.error.is_none() {
                    ui.label(
                        RichText::new("Open a rules file to evaluate it against the CSV.")
                            .font(theme::regular(12.0))
                            .color(t.text_muted),
                    );
                }
            });
    }

    fn rule_card(
        &self,
        ui: &mut Ui,
        t: &theme::Tokens,
        index: usize,
        rule: &RuleReport,
        actions: &mut Vec<RuleAction>,
    ) {
        let status = if rule.passed() { "passed" } else { "failed" };
        let active = self.active_rule() == Some(index);
        let collecting = self.rules.collecting && self.rules.pending_rule == Some(index);
        let selected = active || collecting;
        let filter = if selected { self.rules.hits_filter } else { None };

        // A borderless row that tints on selection, like the PrintCraft tool rows.
        let fill = if selected { t.accent_soft } else { t.hover };
        let stroke = if selected {
            Stroke::new(1.0, t.accent)
        } else {
            Stroke::NONE
        };
        egui::Frame::NONE
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(RADIUS))
            .inner_margin(Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let caret = if selected {
                        "chevron-down"
                    } else {
                        "chevron-right"
                    };
                    let (r, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
                    icons::paint(ui, r, caret, 13.0, t.icon);
                    let label = format!("{}. {}", index + 1, rule.name);
                    let name_w = (ui.available_width() - 70.0).max(60.0);
                    let resp = ui.add_sized(
                        [name_w, 18.0],
                        egui::Label::new(
                            RichText::new(label)
                                .font(theme::medium(14.0))
                                .color(t.text),
                        )
                        .truncate()
                        .sense(Sense::click()),
                    );
                    if resp.clicked() {
                        actions.push(RuleAction::All(index));
                    }
                    let _ = resp.on_hover_text(format!(
                        "{} — show every row of this rule in the grid",
                        rule.name
                    ));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        status_badge(ui, t, status);
                    });
                });
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(6.0, 4.0);
                    ui.label(
                        RichText::new(format!("checked {}", rule.rows_checked))
                            .font(theme::regular(12.0))
                            .color(t.text_muted),
                    );
                    if outcome_pill(
                        ui,
                        t,
                        &format!("passed {}", rule.rows_passed),
                        filter == Some(RowOutcome::Passed),
                        RowOutcome::Passed,
                    )
                    .clicked()
                    {
                        actions.push(RuleAction::Filter(index, RowOutcome::Passed));
                    }
                    if outcome_pill(
                        ui,
                        t,
                        &format!("failed {}", rule.rows_failed),
                        filter == Some(RowOutcome::Failed),
                        RowOutcome::Failed,
                    )
                    .clicked()
                    {
                        actions.push(RuleAction::Filter(index, RowOutcome::Failed));
                    }
                    if outcome_pill(
                        ui,
                        t,
                        &format!("skipped {}", rule.rows_skipped),
                        filter == Some(RowOutcome::Skipped),
                        RowOutcome::Skipped,
                    )
                    .clicked()
                    {
                        actions.push(RuleAction::Filter(index, RowOutcome::Skipped));
                    }
                    if outcome_pill(
                        ui,
                        t,
                        &format!("validation skipped {}", rule.rows_validation_skipped),
                        filter == Some(RowOutcome::ValidationSkipped),
                        RowOutcome::ValidationSkipped,
                    )
                    .clicked()
                    {
                        actions.push(RuleAction::Filter(index, RowOutcome::ValidationSkipped));
                    }
                });
                if active {
                    ui.add_space(4.0);
                    let label = match filter {
                        None => "all rows",
                        Some(RowOutcome::Passed) => "matching rows",
                        Some(RowOutcome::Failed) => "failing rows",
                        Some(RowOutcome::Skipped) => "skipped rows",
                        Some(RowOutcome::ValidationSkipped) => "validation-skipped rows",
                    };
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let (r, _) = ui.allocate_exact_size(vec2(13.0, 13.0), Sense::hover());
                        icons::paint(ui, r, "list-checks", 12.0, t.accent);
                        ui.label(
                            RichText::new(format!("{label} shown in the grid"))
                                .font(theme::regular(11.0))
                                .color(t.text_muted),
                        );
                        if inline_button(ui, t, "clear").clicked() {
                            actions.push(RuleAction::Clear);
                        }
                    });
                } else if collecting {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(13.0, 13.0), Sense::hover());
                        icons::paint(ui, r, "clock-3", 12.0, t.icon);
                        ui.label(
                            RichText::new("collecting rows…")
                                .font(theme::regular(12.0))
                                .color(t.text_muted),
                        );
                    });
                }
            });
    }

    fn apply_rule_action(&mut self, action: RuleAction) {
        match action {
            RuleAction::All(rule) => self.show_rule_rows(rule, None),
            RuleAction::Filter(rule, outcome) => self.show_rule_rows(rule, Some(outcome)),
            RuleAction::Clear => self.clear_rule_view(),
            RuleAction::ToggleAttrsOnly(checked) => self.toggle_rule_attrs_only(checked),
            RuleAction::Close => self.show_rules = false,
            RuleAction::OpenRules => self.open_rules_dialog(),
        }
    }

    fn open_rules_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Rule DSL", &["vl", "rules", "txt"])
            .pick_file()
        {
            self.rules.path = Some(path);
            self.rules.report = None;
            self.rules.plan = None;
            discard_rule_hits(self.rules.hits.take());
            self.rules.queued_rule = None;
            self.rules.error = None;
            self.start_rules_evaluation();
        }
    }

    fn detail_modal(&mut self, ctx: &egui::Context) {
        let Some(detail) = self.detail.clone() else {
            return;
        };
        let t = theme::Tokens::get(ctx);
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
        let panel_width = (ctx.content_rect().width() * 0.55).clamp(360.0, 820.0);
        let label_width = (panel_width * 0.28).clamp(90.0, 170.0);
        // Let the body grow to fit every row; it only scrolls when the rows
        // would exceed the window. The scroll area's `auto_shrink` collapses it
        // to the content height, so no scrollbar shows while there is room.
        let body_height = (ctx.content_rect().height() - 170.0).max(120.0);
        let value_width = (panel_width - label_width - 96.0).max(80.0);
        let mut close = false;
        let mut copy: Option<(String, String)> = None;

        let modal = egui::Modal::new(egui::Id::new("fview-detail"))
            .frame(card_frame(&t).inner_margin(Margin::same(16)))
            .show(ctx, |ui| {
                ui.set_width(panel_width);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let (r, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                    ui.painter().rect(
                        r,
                        CornerRadius::same(8),
                        t.accent_soft,
                        Stroke::NONE,
                        StrokeKind::Inside,
                    );
                    icons::paint(ui, r, "list-checks", 16.0, t.accent_text);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(&detail.title).font(theme::semibold(17.0)),
                        );
                        ui.label(
                            RichText::new(if fields.len() == 1 {
                                "1 attribute".to_string()
                            } else {
                                format!("{} attributes", fields.len())
                            })
                            .font(theme::regular(12.0))
                            .color(t.text_muted),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                        if let Some(attribute) = &self.copy_notice {
                            let (r, _) = ui.allocate_exact_size(vec2(13.0, 13.0), Sense::hover());
                            icons::paint(ui, r, "check", 12.0, t.success);
                            ui.label(
                                RichText::new(format!("copied {attribute}"))
                                    .font(theme::regular(12.0))
                                    .color(t.success),
                            );
                        }
                    });
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .max_height(body_height)
                    .auto_shrink([false, true])
                    .id_salt("fview-detail-body")
                    .show(ui, |ui| {
                        for (name, value) in &fields {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                ui.allocate_ui_with_layout(
                                    vec2(label_width, 26.0),
                                    Layout::right_to_left(Align::Center),
                                    |ui| {
                                        ui.label(
                                            RichText::new(name)
                                                .font(theme::regular(13.0))
                                                .color(t.text_muted),
                                        );
                                    },
                                );
                                egui::Frame::NONE
                                    .fill(t.hover)
                                    .stroke(Stroke::new(1.0, t.border))
                                    .corner_radius(CornerRadius::same(6))
                                    .inner_margin(Margin::symmetric(8, 4))
                                    .show(ui, |ui| {
                                        ui.set_width(value_width);
                                        ui.label(
                                            RichText::new(value)
                                                .font(theme::regular(13.0))
                                                .color(t.text),
                                        );
                                    });
                                if icons::button(ui, "copy", 22.0, false, "Copy value").clicked() {
                                    copy = Some((name.clone(), value.clone()));
                                }
                            });
                            ui.add_space(4.0);
                        }
                    });
            });

        if modal.backdrop_response.clicked() {
            close = true;
        }
        if let Some((attribute, value)) = copy {
            self.copy_value(&attribute, &value, ctx);
        }
        if close {
            self.detail = None;
            self.copy_notice = None;
        }
    }
}

impl eframe::App for Viewer {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.styled {
            egui_extras::install_image_loaders(ctx);
            theme::install_fonts(ctx);
            if ctx.system_theme() == Some(egui::Theme::Dark) {
                self.theme_kind = theme::ThemeKind::Dark;
            }
            theme::apply(ctx, self.theme_kind);
            self.styled = true;
        } else if !self.fonts_ready {
            self.fonts_ready = true;
        }

        self.handle_jobs();

        if self.debounce_pending {
            let quiet = self
                .last_edit
                .map(|at| at.elapsed() >= Duration::from_millis(DEBOUNCE_QUIET_MS))
                .unwrap_or(true);
            if quiet {
                self.debounce_pending = false;
                if self.last_scanned.as_deref() != Some(self.filter.as_str()) {
                    self.start_scan();
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(DEBOUNCE_TICK_MS));
            }
        }
        if self.busy() {
            ctx.request_repaint_after(Duration::from_millis(DEBOUNCE_TICK_MS));
        }

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.escape();
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        if !self.fonts_ready {
            ui.ctx().request_repaint();
            return;
        }
        let ctx = ui.ctx().clone();
        let t = theme::Tokens::get(&ctx);

        if self.path.is_none() {
            let frame = egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(t.pasteboard));
            let this = &mut *self;
            frame.show(ui, |ui| this.welcome(ui));
            return;
        }

        egui::Panel::top("fview-toolbar")
            .exact_size(TOOLBAR_HEIGHT)
            .frame(
                egui::Frame::NONE
                    .fill(t.chrome)
                    .inner_margin(Margin::symmetric(12, 8))
                    .stroke(Stroke::new(1.0, t.divider)),
            )
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::top("fview-status")
            .exact_size(STATUS_HEIGHT)
            .frame(egui::Frame::NONE.fill(t.chrome).inner_margin(Margin::symmetric(12, 0)))
            .show(ui, |ui| self.status_bar(ui));
        if self.show_config {
            self.config_panel(ui);
        }

        if self.show_rules {
            self.rules_panel(ui);
        }

        let frame = egui::CentralPanel::default().frame(
            egui::Frame::NONE
                .fill(t.pasteboard)
                .inner_margin(Margin::same(CONTENT_PADDING as i8)),
        );
        frame.show(ui, |ui| self.grid(ui));

        self.detail_modal(&ctx);
    }
}

/// A clickable outcome count (`passed N`, `failed N`, …).
fn outcome_pill(
    ui: &mut Ui,
    t: &theme::Tokens,
    label: &str,
    active: bool,
    outcome: RowOutcome,
) -> egui::Response {
    let font = theme::regular(12.0);
    let text_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(label.to_owned(), font.clone(), t.text)
            .size()
            .x
    });
    let (rect, resp) = ui.allocate_exact_size(vec2(text_w + 20.0, 22.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label));
    let (weak, strong) = outcome_colors(t, outcome);
    let (fill, stroke, text) = if active {
        (weak, Stroke::new(1.0, strong), strong)
    } else if resp.hovered() {
        (t.hover, Stroke::new(1.0, t.border), t.text)
    } else {
        (Color32::TRANSPARENT, Stroke::new(1.0, t.border), t.text)
    };
    ui.painter()
        .rect(rect, CornerRadius::same(11), fill, stroke, StrokeKind::Inside);
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, label, font, text);
    resp
}

/// A tinted pill for a rule's `passed` / `failed` status.
fn status_badge(ui: &mut Ui, t: &theme::Tokens, status: &str) {
    let font = theme::regular(11.0);
    let text_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(status.to_owned(), font.clone(), t.text)
            .size()
            .x
    });
    let (rect, _) = ui.allocate_exact_size(vec2(text_w + 16.0, 20.0), Sense::hover());
    let (fill, text) = if status == "passed" {
        (t.success_soft, t.success)
    } else {
        (t.danger_soft, t.danger)
    };
    ui.painter()
        .rect(rect, CornerRadius::same(10), fill, Stroke::NONE, StrokeKind::Inside);
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, status, font, text);
}

/// The soft (fill) and strong (text/border) colours for a row outcome.
fn outcome_colors(t: &theme::Tokens, outcome: RowOutcome) -> (Color32, Color32) {
    match outcome {
        RowOutcome::Passed => (t.success_soft, t.success),
        RowOutcome::Failed => (t.danger_soft, t.danger),
        RowOutcome::Skipped => (t.hover, t.text_muted),
        RowOutcome::ValidationSkipped => (t.accent_soft, t.accent_text),
    }
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

/// Memory-map a file read-only. Returns `None` for a zero-length file.
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
/// per-column mask.
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
/// only the bounded sample. `hits_limit` caps the retained rows per outcome.
fn run_plan(
    plan: &Plan,
    csv: &Path,
    delimiter: u8,
    collect_hits: Option<usize>,
    hits_limit: usize,
) -> Result<Report, String> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let config = EngineConfig {
        path: csv.to_path_buf(),
        delimiter,
        threads,
        // The viewer keeps row numbers as the row id: rules are evaluated in
        // parallel.
        id_idx: None,
        progress: None,
        collect_hits,
        collect_hits_limit: hits_limit,
    };
    engine::run(plan, &config)
}

/// Release a collected row list without blocking the UI thread.
fn discard_rule_hits(hits: Option<RuleHits>) {
    if let Some(hits) = hits {
        rayon::spawn(move || drop(hits));
    }
}

/// Collect one rule's rows for the grid. The compiled plan is filtered down to
/// that rule before the engine runs.
fn collect_rule_hits(
    plan: Arc<Plan>,
    rule: usize,
    csv: PathBuf,
    delimiter: u8,
    limit: usize,
) -> Result<RuleHits, String> {
    let compiled = plan
        .rules
        .get(rule)
        .cloned()
        .ok_or_else(|| format!("rule {} not found", rule + 1))?;
    let single = Plan {
        rules: vec![compiled],
    };
    let mut report = run_plan(&single, &csv, delimiter, Some(0), limit)?;
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
    })
}

/// Stream the whole file, count every match and keep the first `limit` rows.
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
        // The exact total is unknown because the scan stopped early.
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
        // `position` is the start of the record that is about to be read.
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

/// Prefix ("beginsWith") search served by the built column indexes.
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

fn main() -> eframe::Result {
    let args = Args::parse();
    let viewport = egui::ViewportBuilder::default()
        .with_title("fview — CSV viewer")
        .with_inner_size([1200.0, 820.0])
        .with_min_inner_size([720.0, 480.0])
        .with_drag_and_drop(true);
    let native = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "fview",
        native,
        Box::new(move |_cc| Ok(Box::new(Viewer::new(args)))),
    )
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
        assert_eq!(
            status_text(100, 714, 5000, true, false, None),
            "showing first 100 of 714 matching rows · 5000 rows read"
        );
        assert_eq!(
            status_text(7, 7, 500, false, false, None),
            "7 matching rows of 500 total"
        );
        assert_eq!(status_text(500, 500, 500, false, false, None), "500 rows");
        assert_eq!(
            status_text(100, 714, 714, true, true, None),
            "showing first 100 of 714 matching rows (index prefix)"
        );
        assert_eq!(
            status_text(7, 7, 7, false, true, None),
            "7 matching rows (index prefix)"
        );
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

        let limited = scan(path.clone(), b',', String::new(), false, 2, None, false).unwrap();
        assert_eq!(limited.rows.len(), 2);
        assert_eq!(limited.matched, 2);
        assert!(limited.truncated);
        assert_eq!(limited.rows_read, 3);

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

        let all = scan(path.clone(), b',', "foo".into(), false, 10, None, false).unwrap();
        assert_eq!(all.rows.len(), 2);

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

        let bytes = std::fs::read(&path).unwrap();
        assert!(segments_for(&bytes, b',', 4).unwrap().len() > 1);

        let sequential =
            scan(path.clone(), b',', "city3".into(), false, 10, None, false).unwrap();
        let parallel = scan(path.clone(), b',', "city3".into(), false, 10, None, true).unwrap();

        assert_eq!(sequential.rows, parallel.rows);
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
        assert_eq!(index.prefix_offsets("ap"), vec![10, 30]);
        assert_eq!(index.prefix_offsets("ban"), vec![20]);
        assert_eq!(index.prefix_offsets("APPLE"), vec![30]);
        assert!(index.prefix_offsets("zzz").is_empty());
    }

    #[test]
    fn build_index_offsets_resolve_to_the_right_rows() {
        let path = std::env::temp_dir().join(format!("fview-index-{}.csv", std::process::id()));
        std::fs::write(&path, "name,city\nAlice,Paris\nBob,Lyon\nAnna,Nice\n").unwrap();
        let bytes = std::fs::read(&path).unwrap();

        let index = build_index(path.clone(), b',', 0).unwrap();
        let offsets = index.prefix_offsets("an");
        assert_eq!(offsets.len(), 1);
        let rows = rows_at_offsets(&bytes, b',', &offsets, 10).unwrap();
        assert_eq!(rows, vec![vec!["Anna".to_string(), "Nice".to_string()]]);

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

        let none = scan_indexed(path.clone(), b',', "li".into(), vec![index], 10).unwrap();
        assert_eq!(none.matched, 0);
        assert!(none.rows.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stripe_height_grows_with_line_count() {
        assert!(stripe_height(1) > 0.0);
        assert!(stripe_height(3) > stripe_height(2));
        assert!(stripe_height(2) > stripe_height(1));
        assert_eq!(stripe_height(0), stripe_height(1));
    }

    #[test]
    fn chip_lines_fill_each_line_before_wrapping() {
        let available = 260.0;
        let widths = [100.0, 100.0, 100.0];
        let lines = chip_lines(&widths, available);
        assert_eq!(lines, vec![0..2, 2..3]);
        assert_eq!(chip_line_count(widths.iter().copied(), available), lines.len());

        let wide = [available + 50.0];
        assert_eq!(chip_line_count(wide.iter().copied(), available), 1);
        assert_eq!(chip_lines(&wide, available), vec![0..1]);

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
            let covered: Vec<usize> = ranges.iter().flat_map(|range| range.clone()).collect();
            assert_eq!(covered, (0..widths.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn rule_view_can_render_full_rows_from_engine() {
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
        let hits = collect_rule_hits(plan, 0, path, b',', usize::MAX).unwrap();
        assert_eq!(hits.hits.len(), 3);
        assert_eq!(*hits.rows[0], vec!["1".to_string(), "Alice".to_string()]);
        assert_eq!(*hits.rows[2], vec!["3".to_string(), "Cara".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collecting_one_rule_ignores_the_others() {
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

        let a = collect_rule_hits(Arc::clone(&plan), 0, path.clone(), b',', usize::MAX).unwrap();
        assert_eq!(a.hits.len(), 2);
        assert_eq!(a.hits.iter().filter(|hit| hit.passed()).count(), 1);
        assert_eq!(a.passed, 1);
        assert_eq!(a.failed, 1);

        let id = collect_rule_hits(plan, 1, path, b',', usize::MAX).unwrap();
        assert_eq!(id.hits.len(), 2);
        assert!(id.hits.iter().all(|hit| !hit.passed()));
        assert_eq!(id.failed, 2);

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
        viewer.show_rule_rows(1, None);
        assert_eq!(viewer.rules.queued_rule, Some((1, None)));
        assert_eq!(viewer.rules.pending_rule, Some(0));

        // Clicking the rule already being collected drops the queued one.
        viewer.show_rule_rows(0, Some(RowOutcome::Failed));
        assert_eq!(viewer.rules.queued_rule, None);
        assert_eq!(viewer.rules.hits_filter, Some(RowOutcome::Failed));
    }

    /// A minimal viewer with three headers and no file.
    fn test_viewer() -> Viewer {
        let mut viewer = Viewer::new(Args {
            path: None,
            delimiter: ",".into(),
            case_sensitive: false,
            limit: 100,
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
        viewer.apply_rule_view();
        assert_eq!(viewer.rows.len(), 1);
        // Switching outcome re-uses the cached record: same allocation.
        assert!(Arc::ptr_eq(&viewer.rows[0], &row));
    }

    #[test]
    fn changing_limit_keeps_the_rule_filter() {
        let mut viewer = test_viewer();
        viewer.rules.hits = Some(RuleHits {
            rule: 0,
            cap: 100,
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

        viewer.set_limit(2);

        // The rule/outcome filter survives a limit change.
        assert!(viewer.rules.view_active);
        assert_eq!(viewer.rows.len(), 2);
        assert_eq!(viewer.matched, 5);
        assert!(viewer.truncated);
    }

    #[test]
    fn locked_columns_survive_mute_all() {
        let mut viewer = test_viewer();
        viewer.locked.insert(1);

        viewer.mute_all();
        assert!(!viewer.muted.contains(&1), "a locked column stays visible");
        assert!(viewer.muted.contains(&0));
        assert!(viewer.muted.contains(&2));

        // Hiding a locked column directly is ignored too.
        viewer.mute(1);
        assert!(!viewer.muted.contains(&1));

        // Unlocking keeps it visible; hiding it is a separate action.
        viewer.toggle_lock(1);
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

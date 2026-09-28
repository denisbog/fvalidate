//! Report data model and rendering (human text + JSON).

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Example {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row: Option<u64>,
    pub left: String,
    pub right: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetExample {
    pub value: String,
    pub count: u64,
}

/// One row that matched or failed a rule. Unlike [`GroupedExample`], these are
/// collected for *every* row (used by the GUI's "show all rows" action) and
/// carry the transformed values that were compared.
#[derive(Debug, Clone, Serialize)]
pub struct RowHit {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row: Option<u64>,
    pub left: String,
    pub right: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// `true` when the row matched the rule, `false` when it failed.
    pub passed: bool,
}

/// Matching or failing rows aggregated by their `(left, right, expected)`
/// values. `count` is the number of rows in the group and `ids` holds up to
/// `report_limit` example ids.
#[derive(Debug, Clone, Serialize)]
pub struct GroupedExample {
    pub left: String,
    pub right: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    pub count: u64,
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MappingEntry {
    pub input: String,
    pub canonical: String,
    pub ambiguous: bool,
    pub targets: Vec<TargetExample>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AmbiguityReport {
    pub input: String,
    pub targets: Vec<TargetExample>,
    pub examples: Vec<Example>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MappingReport {
    /// `auto` or `file`.
    pub source: String,
    pub distinct_inputs: usize,
    pub ambiguous_inputs: usize,
    /// Complete extracted/loaded relation.
    pub entries: Vec<MappingEntry>,
    pub ambiguities: Vec<AmbiguityReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleReport {
    pub name: String,
    pub left: String,
    pub right: String,
    /// `passed` or `failed`.
    pub status: String,
    pub rows_checked: u64,
    pub rows_passed: u64,
    pub rows_failed: u64,
    pub rows_skipped: u64,
    /// Rows skipped by an explicit \`validation_skipped\` predicate.
    pub rows_validation_skipped: u64,
    pub transform_errors: u64,
    pub unmapped_values: u64,
    /// Matching rows aggregated by value, most frequent first (up to the
    /// report limit).
    pub pass_results: Vec<GroupedExample>,
    /// Failing rows aggregated by value, most frequent first (up to the
    /// report limit).
    pub fail_results: Vec<GroupedExample>,
    /// Rows skipped by the explicit `validation_skipped` predicate, aggregated
    /// by value like the matching and failing results.
    pub validation_skipped_results: Vec<GroupedExample>,
    /// Column indices referenced by the rule (left/right/derive/predicates).
    /// GUI-only: used to show just the attributes a rule looks at. Not part of
    /// the serialized report contract.
    #[allow(dead_code)]
    #[serde(skip)]
    pub rule_columns: Vec<usize>,
    /// Every matching and failing row, in file order. Empty unless the engine
    /// was asked to collect the hits for this rule (GUI "show all rows").
    #[allow(dead_code)]
    #[serde(skip)]
    pub hits: Vec<RowHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping: Option<MappingReport>,
}

impl RuleReport {
    pub fn passed(&self) -> bool {
        self.status == "passed"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub rules_total: usize,
    pub rules_passed: usize,
    pub rules_failed: usize,
    pub rows_checked: u64,
    pub rules: Vec<RuleReport>,
}

impl Report {
    pub fn render_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    pub fn render_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();

        let width = 68;
        let _ = writeln!(out, "{}", "=".repeat(width));
        let _ = writeln!(out, " CSV validation report");
        let _ = writeln!(out, "{}", "=".repeat(width));
        let _ = writeln!(out, "Rules processed : {}", self.rules_total);
        let _ = writeln!(out, "Rules passed    : {}", self.rules_passed);
        let _ = writeln!(out, "Rules failed    : {}", self.rules_failed);
        let _ = writeln!(out, "Rows checked    : {}", self.rows_checked);
        let _ = writeln!(out);

        for (index, rule) in self.rules.iter().enumerate() {
            let _ = writeln!(
                out,
                "[{}] \"{}\"  {}",
                index + 1,
                rule.name,
                rule.status.to_uppercase()
            );
            let _ = writeln!(out, "    left  : {}", rule.left);
            let _ = writeln!(out, "    right : {}", rule.right);
            let _ = writeln!(
                out,
                "    checked={} passed={} failed={} skipped={} validation_skipped={} transform_errors={} unmapped_values={}",
                rule.rows_checked,
                rule.rows_passed,
                rule.rows_failed,
                rule.rows_skipped,
                rule.rows_validation_skipped,
                rule.transform_errors,
                rule.unmapped_values
            );

            if !rule.pass_results.is_empty() {
                let _ = writeln!(out, "    matching results ({}):", rule.pass_results.len());
                for group in &rule.pass_results {
                    let _ = writeln!(out, "      {}", format_group(group));
                }
            }
            if !rule.fail_results.is_empty() {
                let _ = writeln!(out, "    failed results ({}):", rule.fail_results.len());
                for group in &rule.fail_results {
                    let _ = writeln!(out, "      {}", format_group(group));
                }
            }
            if !rule.validation_skipped_results.is_empty() {
                let _ = writeln!(
                    out,
                    "    validation-skipped results ({}):",
                    rule.validation_skipped_results.len()
                );
                for group in &rule.validation_skipped_results {
                    let _ = writeln!(out, "      {}", format_group(group));
                }
            }

            if let Some(mapping) = &rule.mapping {
                let _ = writeln!(
                    out,
                    "    mapping ({}): {} distinct inputs, {} ambiguous",
                    mapping.source, mapping.distinct_inputs, mapping.ambiguous_inputs
                );

                for ambiguity in &mapping.ambiguities {
                    let targets: Vec<String> = ambiguity
                        .targets
                        .iter()
                        .map(|t| format!("{} ({})", quote(&t.value), t.count))
                        .collect();
                    let _ = writeln!(
                        out,
                        "      ambiguous input {} -> {}",
                        quote(&ambiguity.input),
                        targets.join(", ")
                    );
                    for example in &ambiguity.examples {
                        let _ = writeln!(out, "        example: {}", format_example(example));
                    }
                }

                if !mapping.entries.is_empty() {
                    let _ = writeln!(out, "    full mapping:");
                    for entry in &mapping.entries {
                        let mut targets = String::new();
                        for (i, target) in entry.targets.iter().enumerate() {
                            if i > 0 {
                                targets.push_str(", ");
                            }
                            let _ = write!(targets, "{} ({})", quote(&target.value), target.count);
                        }
                        let flag = if entry.ambiguous { " [ambiguous]" } else { "" };
                        let _ = writeln!(
                            out,
                            "      {} -> {}{}  {{{}}}",
                            quote(&entry.input),
                            quote(&entry.canonical),
                            flag,
                            targets
                        );
                    }
                }
            }

            let _ = writeln!(out);
        }

        out
    }

    /// Render a self-contained HTML page (embedded CSS, no external assets).
    pub fn render_html(&self, title: &str) -> String {
        use std::fmt::Write as _;

        let mut out = String::with_capacity(16 * 1024);
        let title = html_escape(title);

        let _ = write!(
            out,
            "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
             <title>{title}</title>\n<style>{css}</style>\n</head>\n<body>\n<main>\n\
             <h1>{title}</h1>\n<p class=\"subtitle\">Generated by <code>fvalidate</code></p>\n",
            title = title,
            css = HTML_CSS,
        );

        let _ = writeln!(
            out,
            "<section class=\"summary\">\
             <div class=\"stat\"><span class=\"value\">{total}</span><span class=\"label\">rules</span></div>\
             <div class=\"stat ok\"><span class=\"value\">{passed}</span><span class=\"label\">passed</span></div>\
             <div class=\"stat bad\"><span class=\"value\">{failed}</span><span class=\"label\">failed</span></div>\
             <div class=\"stat\"><span class=\"value\">{rows}</span><span class=\"label\">rows</span></div>\
             </section>",
            total = self.rules_total,
            passed = self.rules_passed,
            failed = self.rules_failed,
            rows = self.rows_checked,
        );

        // Outline: one row per rule, sorted by the target column, summarising
        // the validation result and linking to the rule's section.
        let mut order: Vec<usize> = (0..self.rules.len()).collect();
        order.sort_by(|&a, &b| {
            let ra = &self.rules[a];
            let rb = &self.rules[b];
            ra.right
                .to_lowercase()
                .cmp(&rb.right.to_lowercase())
                .then_with(|| ra.left.to_lowercase().cmp(&rb.left.to_lowercase()))
                .then_with(|| a.cmp(&b))
        });

        let _ = writeln!(
            out,
            "<nav class=\"outline\" id=\"outline\"><h2>Outline</h2>\
             <div class=\"table-scroll\"><table><thead><tr>\
             <th>target</th><th>source</th><th>rule</th><th>mapping</th><th>status</th>\
             <th>checked</th><th>passed</th><th>failed</th><th>skipped</th>\
             <th>validation skipped</th>\
             </tr></thead><tbody>"
        );
        for &index in &order {
            let rule = &self.rules[index];
            let status = if rule.passed() { "passed" } else { "failed" };
            let mapping = match &rule.mapping {
                Some(mapping) => format!(
                    "<span class=\"map {source}\">{label}</span>",
                    source = html_escape(&mapping.source),
                    label = html_escape(&mapping.source),
                ),
                None => "<span class=\"map none\">&mdash;</span>".to_string(),
            };
            let _ = writeln!(
                out,
                "<tr><td><code>{target}</code></td><td><code>{source}</code></td>\
                 <td><a href=\"#rule-{anchor}\">{name}</a></td>\
                 <td>{mapping}</td>\
                 <td><span class=\"badge {status}\">{status}</span></td>\
                 <td>{checked}</td><td class=\"pass\">{passed}</td>\
                 <td class=\"fail\">{failed}</td><td>{skipped}</td>\
                 <td>{validation_skipped}</td></tr>",
                target = html_escape(&rule.right),
                source = html_escape(&rule.left),
                anchor = index + 1,
                name = html_escape(&rule.name),
                mapping = mapping,
                checked = rule.rows_checked,
                passed = rule.rows_passed,
                failed = rule.rows_failed,
                skipped = rule.rows_skipped,
                validation_skipped = rule.rows_validation_skipped,
            );
        }
        out.push_str("</tbody></table></div></nav>\n");

        for (index, rule) in self.rules.iter().enumerate() {
            let status = if rule.passed() { "passed" } else { "failed" };
            let anchor = index + 1;

            let _ = writeln!(
                out,
                "<section class=\"rule {status}\" id=\"rule-{anchor}\">"
            );
            let _ = writeln!(
                out,
                "<header><h2>{anchor}. {name}</h2>\
                 <div class=\"actions\"><span class=\"badge {status}\">{status}</span>\
                 <a class=\"back\" href=\"#outline\">&uarr; outline</a></div></header>",
                name = html_escape(&rule.name),
            );
            let _ = writeln!(
                out,
                "<p class=\"columns\">left <code>{}</code> &rarr; right <code>{}</code></p>",
                html_escape(&rule.left),
                html_escape(&rule.right),
            );
            let _ = writeln!(
                out,
                "<div class=\"stats\"><span>checked <b>{}</b></span>\
                 <span class=\"pass\">passed <b>{}</b></span>\
                 <span class=\"fail\">failed <b>{}</b></span>\
                 <span>skipped <b>{}</b></span>\
                 <span>validation skipped <b>{}</b></span>\
                 <span>transform errors <b>{}</b></span>\
                 <span>unmapped values <b>{}</b></span></div>",
                rule.rows_checked,
                rule.rows_passed,
                rule.rows_failed,
                rule.rows_skipped,
                rule.rows_validation_skipped,
                rule.transform_errors,
                rule.unmapped_values,
            );

            if !rule.pass_results.is_empty() {
                let _ = writeln!(
                    out,
                    "<h3 class=\"pass\">Matching results ({})</h3>",
                    rule.pass_results.len()
                );
                out.push_str(&html_grouped_table(&rule.pass_results));
            }
            if !rule.fail_results.is_empty() {
                let _ = writeln!(
                    out,
                    "<h3 class=\"fail\">Failed results ({})</h3>",
                    rule.fail_results.len()
                );
                out.push_str(&html_grouped_table(&rule.fail_results));
            }
            if !rule.validation_skipped_results.is_empty() {
                let _ = writeln!(
                    out,
                    "<h3 class=\"skip\">Validation-skipped results ({})</h3>",
                    rule.validation_skipped_results.len()
                );
                out.push_str(&html_grouped_table(&rule.validation_skipped_results));
            }

            if let Some(mapping) = &rule.mapping {
                let _ = writeln!(
                    out,
                    "<div class=\"mapping\"><h3>Mapping <span class=\"tag\">{}</span></h3>\
                     <p>{} distinct inputs, <b>{}</b> ambiguous</p>",
                    html_escape(&mapping.source),
                    mapping.distinct_inputs,
                    mapping.ambiguous_inputs,
                );

                for ambiguity in &mapping.ambiguities {
                    let _ = write!(
                        out,
                        "<div class=\"ambiguity\"><p>Ambiguous input <code>{}</code></p>\
                         <p class=\"targets\">",
                        html_escape(&ambiguity.input),
                    );
                    for target in &ambiguity.targets {
                        let _ = write!(
                            out,
                            "<span class=\"chip\">{} <b>{}</b></span>",
                            html_escape(&target.value),
                            target.count,
                        );
                    }
                    out.push_str("</p>");
                    if !ambiguity.examples.is_empty() {
                        out.push_str(&html_examples_table(&ambiguity.examples));
                    }
                    out.push_str("</div>");
                }

                if !mapping.entries.is_empty() {
                    let _ = write!(
                        out,
                        "<details><summary>Full mapping ({} inputs)</summary>",
                        mapping.entries.len(),
                    );
                    out.push_str(&html_mapping_table(&mapping.entries));
                    out.push_str("</details>");
                }

                out.push_str("</div>");
            }

            out.push_str("</section>\n");
        }

        out.push_str("</main>\n</body>\n</html>\n");
        out
    }
}

const HTML_CSS: &str = r#"
:root { color-scheme: light dark; }
* { box-sizing: border-box; }
body { margin: 0; font: 15px/1.55 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; background: #f5f6f8; color: #1f2430; }
main { max-width: 1200px; margin: 0 auto; padding: 24px 16px 64px; }
h1 { margin: 0 0 4px; font-size: 26px; }
.subtitle { margin: 0 0 20px; color: #6b7280; }
code { background: rgba(127,127,127,.16); padding: 1px 5px; border-radius: 4px; font-size: .9em; }
.summary { display: flex; flex-wrap: wrap; gap: 12px; margin-bottom: 24px; }
.stat { background: #fff; border: 1px solid #e3e6ea; border-radius: 10px; padding: 12px 18px; min-width: 110px; display: flex; flex-direction: column; }
.stat .value { font-size: 24px; font-weight: 700; }
.stat .label { color: #6b7280; font-size: 12px; text-transform: uppercase; letter-spacing: .04em; }
.stat.ok .value { color: #15803d; }
.stat.bad .value { color: #b91c1c; }
html { scroll-behavior: smooth; }
.outline { background: #fff; border: 1px solid #e3e6ea; border-radius: 10px; padding: 12px 16px 6px; margin-bottom: 24px; }
.outline h2 { font-size: 13px; margin: 0 0 8px; text-transform: uppercase; letter-spacing: .04em; color: #6b7280; }
.outline table { font-size: 13px; }
.outline a { color: #4338ca; text-decoration: none; }
.outline a:hover { text-decoration: underline; }
.outline .pass { color: #15803d; }
.outline .fail { color: #b91c1c; }
.table-scroll { overflow-x: auto; }
.rule { background: #fff; border: 1px solid #e3e6ea; border-left: 5px solid #9ca3af; border-radius: 10px; padding: 16px 18px; margin-bottom: 18px; }
.rule.passed { border-left-color: #15803d; }
.rule.failed { border-left-color: #b91c1c; }
.rule header { display: flex; align-items: center; gap: 10px; }
.rule h2 { margin: 0; font-size: 18px; }
.rule header .actions { margin-left: auto; display: flex; align-items: center; gap: 10px; }
.back { font-size: 12px; color: #4338ca; text-decoration: none; white-space: nowrap; }
.back:hover { text-decoration: underline; }
.badge { padding: 2px 10px; border-radius: 999px; font-size: 12px; font-weight: 700; text-transform: uppercase; }
.badge.passed { background: #dcfce7; color: #15803d; }
.badge.failed { background: #fee2e2; color: #b91c1c; }
.columns { color: #4b5563; margin: 6px 0 10px; }
.stats { display: flex; flex-wrap: wrap; gap: 14px; margin-bottom: 4px; color: #4b5563; }
.stats .pass b, h3.pass { color: #15803d; }
.stats .fail b, h3.fail { color: #b91c1c; }
h3 { font-size: 13px; margin: 16px 0 6px; text-transform: uppercase; letter-spacing: .04em; color: #6b7280; }
table { width: 100%; border-collapse: collapse; font-size: 13px; }
th, td { text-align: left; padding: 5px 8px; border-bottom: 1px solid #eceef1; vertical-align: top; }
th { color: #6b7280; font-weight: 600; }
tbody tr:nth-child(odd) { background: #fafbfc; }
.mapping { margin-top: 14px; border-top: 1px dashed #dfe3e8; padding-top: 10px; }
.tag { font-size: 11px; background: #eef2ff; color: #4338ca; padding: 1px 7px; border-radius: 999px; text-transform: uppercase; }
.map { display: inline-block; padding: 1px 8px; border-radius: 999px; font-size: 11px; font-weight: 600; letter-spacing: .03em; text-transform: uppercase; }
.map.auto { background: #eef2ff; color: #4338ca; }
.map.file { background: #fef3c7; color: #92400e; }
.map.none { color: #9ca3af; }
h3.skip { color: #a16207; }
.ambiguity { background: #fffbeb; border: 1px solid #fde68a; border-radius: 8px; padding: 8px 12px; margin: 8px 0; }
.ambiguity p { margin: 4px 0; }
.targets { display: flex; flex-wrap: wrap; gap: 6px; }
.chip { background: #fff; border: 1px solid #e3e6ea; border-radius: 999px; padding: 1px 8px; font-size: 12px; }
.chip b { color: #6b7280; }
details { margin-top: 10px; }
summary { cursor: pointer; color: #4338ca; }
@media (prefers-color-scheme: dark) {
  body { background: #10131a; color: #e5e7eb; }
  .stat, .rule, .outline { background: #171b24; border-color: #2a3040; }
  th, td { border-color: #2a3040; }
  tbody tr:nth-child(odd) { background: #1b202b; }
  .chip { background: #1b202b; border-color: #2a3040; }
  .map.auto { background: #1e1b4b; color: #c7d2fe; }
  .map.file { background: #3a2e12; color: #fde68a; }
  .ambiguity { background: #2a2410; border-color: #6b5b1e; }
  .rule .columns, .stats, .columns { color: #9ca3af; }
}
"#;

fn html_examples_table(examples: &[Example]) -> String {
    let mut out = String::new();
    out.push_str(
        "<table><thead><tr><th>row</th><th>id</th><th>left</th><th>right</th><th>expected</th></tr></thead><tbody>",
    );
    for example in examples {
        out.push_str("<tr>");
        out.push_str(&format!(
            "<td>{}</td>",
            example.row.map(|r| r.to_string()).unwrap_or_default()
        ));
        out.push_str(&format!("<td>{}</td>", html_escape(&example.id)));
        out.push_str(&format!("<td>{}</td>", html_escape(&example.left)));
        out.push_str(&format!("<td>{}</td>", html_escape(&example.right)));
        out.push_str(&format!(
            "<td>{}</td>",
            html_escape(example.expected.as_deref().unwrap_or(""))
        ));
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
    out
}

fn html_grouped_table(groups: &[GroupedExample]) -> String {
    let mut out = String::new();
    out.push_str(
        "<table><thead><tr><th>count</th><th>left</th><th>right</th><th>expected</th><th>ids</th></tr></thead><tbody>",
    );
    for group in groups {
        let ids: Vec<String> = group.ids.iter().map(|id| html_escape(id)).collect();
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            group.count,
            html_escape(&group.left),
            html_escape(&group.right),
            html_escape(group.expected.as_deref().unwrap_or("")),
            ids.join(", "),
        ));
    }
    out.push_str("</tbody></table>");
    out
}

fn html_mapping_table(entries: &[MappingEntry]) -> String {
    let mut out = String::new();
    out.push_str(
        "<table><thead><tr><th>input</th><th>canonical</th><th>ambiguous</th><th>all targets</th></tr></thead><tbody>",
    );
    for entry in entries {
        let targets: Vec<String> = entry
            .targets
            .iter()
            .map(|target| format!("{} ({})", html_escape(&target.value), target.count))
            .collect();
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            html_escape(&entry.input),
            html_escape(&entry.canonical),
            if entry.ambiguous { "yes" } else { "no" },
            targets.join(", "),
        ));
    }
    out.push_str("</tbody></table>");
    out
}

fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn format_group(group: &GroupedExample) -> String {
    let mut out = String::new();
    out.push_str(&format!("count={} ", group.count));
    out.push_str(&format!("left={}", quote(&group.left)));
    out.push_str(&format!(" right={}", quote(&group.right)));
    if let Some(expected) = &group.expected {
        out.push_str(&format!(" expected={}", quote(expected)));
    }
    let ids: Vec<String> = group.ids.iter().map(|id| quote(id)).collect();
    out.push_str(&format!(" ids=[{}]", ids.join(", ")));
    out
}

fn format_example(example: &Example) -> String {
    let mut out = String::new();
    if let Some(row) = example.row {
        out.push_str(&format!("row={} ", row));
    }
    out.push_str(&format!("id={}", quote(&example.id)));
    out.push_str(&format!(" left={}", quote(&example.left)));
    out.push_str(&format!(" right={}", quote(&example.right)));
    if let Some(expected) = &example.expected {
        out.push_str(&format!(" expected={}", quote(expected)));
    }
    out
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

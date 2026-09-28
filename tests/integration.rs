//! End-to-end tests running the compiled `fvalidate` binary.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;

fn manifest(relative: &str) -> String {
    format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative)
}

fn run(args: &[&str]) -> (String, String, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_fvalidate"))
        .args(args)
        .output()
        .expect("failed to run fvalidate");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

fn run_json(args: &[&str]) -> Value {
    let (stdout, stderr, ok) = run(args);
    assert!(ok, "fvalidate failed: {stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid json: {e}\n{stdout}"))
}

fn people_rules() -> String {
    manifest("examples/rules.vl")
}

#[test]
fn reports_counts_and_ambiguities() {
    let rules = people_rules();
    let report = run_json(&[
        &manifest("examples/people.csv"),
        "-r",
        &rules,
        "--id-column",
        "id",
        "--format",
        "json",
        "--no-fail",
    ]);

    assert_eq!(report["rules_total"], 4);
    assert_eq!(report["rows_checked"], 8);

    let rules = report["rules"].as_array().unwrap();

    // Dates: 3 failures (two mismatches + one unparseable).
    let dates = &rules[0];
    assert_eq!(dates["rows_passed"], 5);
    assert_eq!(dates["rows_failed"], 3);
    assert_eq!(dates["transform_errors"], 1);
    assert_eq!(dates["status"], "failed");

    // Country mapping is auto-extracted and ambiguous for "france".
    let country = &rules[1];
    assert_eq!(country["mapping"]["source"], "auto");
    assert_eq!(country["mapping"]["distinct_inputs"], 4);
    assert_eq!(country["mapping"]["ambiguous_inputs"], 1);
    assert_eq!(country["mapping"]["ambiguities"][0]["input"], "france");

    // Multi-value comparison is order independent (b;c == c;b).
    let tags = &rules[2];
    assert_eq!(tags["rows_passed"], 7);
    assert_eq!(tags["rows_failed"], 1);

    // File mapping reports its ambiguity too.
    let city = &rules[3];
    assert_eq!(city["mapping"]["source"], "file");
    assert_eq!(city["mapping"]["ambiguous_inputs"], 1);
    assert_eq!(city["rows_failed"], 0);
    assert_eq!(city["status"], "failed");
}

#[test]
fn example_limit_is_respected() {
    let rules = people_rules();
    let report = run_json(&[
        &manifest("examples/people.csv"),
        "-r",
        &rules,
        "--id-column",
        "id",
        "-n",
        "2",
        "--format",
        "json",
        "--no-fail",
    ]);

    for rule in report["rules"].as_array().unwrap() {
        assert!(rule["pass_results"].as_array().unwrap().len() <= 2);
        assert!(rule["fail_results"].as_array().unwrap().len() <= 2);
        if let Some(mapping) = rule.get("mapping") {
            if mapping.get("ambiguities").is_some() {
                assert!(mapping["ambiguities"].as_array().unwrap().len() <= 2);
            }
        }
    }
}

#[test]
fn exits_nonzero_when_a_rule_fails() {
    let rules = people_rules();
    let (_, _, ok) = run(&[
        &manifest("examples/people.csv"),
        "-r",
        &rules,
        "--id-column",
        "id",
    ]);
    assert!(!ok, "expected a non-zero exit code");
}

#[test]
fn reads_from_stdin() {
    let csv = std::fs::read_to_string(manifest("examples/people.csv")).unwrap();
    let rules = people_rules();

    let mut child = Command::new(env!("CARGO_BIN_EXE_fvalidate"))
        .args([
            "-",
            "-r",
            &rules,
            "--id-column",
            "id",
            "--format",
            "json",
            "--no-fail",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(csv.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["rows_checked"], 8);
}

/// A composite key built from two input columns must be validated as a pair.
#[test]
fn composite_keys_use_two_input_values() {
    let report = run_json(&[
        &manifest("examples/orders.csv"),
        "-r",
        &manifest("examples/rules_composite.vl"),
        "--id-column",
        "order_id",
        "--format",
        "json",
        "--no-fail",
    ]);

    assert_eq!(report["rows_checked"], 7);
    let rules = report["rules"].as_array().unwrap();

    // Reference-table lookup keyed by (product, region).
    let reference = &rules[0];
    assert_eq!(reference["left"], "product + region");
    assert_eq!(reference["mapping"]["source"], "file");
    assert_eq!(reference["mapping"]["distinct_inputs"], 5);
    assert_eq!(reference["rows_passed"], 5);
    assert_eq!(reference["rows_failed"], 2);
    assert_eq!(reference["mapping"]["entries"][0]["input"], "gadget|eu");

    // Same key extracted from the data: the noisy rows make it ambiguous.
    let auto = &rules[1];
    assert_eq!(auto["mapping"]["source"], "auto");
    assert_eq!(auto["mapping"]["ambiguous_inputs"], 2);
    assert_eq!(auto["rows_failed"], 2);
}

#[test]
fn html_report_is_self_contained() {
    let rules = people_rules();
    let (stdout, stderr, ok) = run(&[
        &manifest("examples/people.csv"),
        "-r",
        &rules,
        "--id-column",
        "id",
        "--format",
        "html",
        "--no-fail",
    ]);
    assert!(ok, "fvalidate failed: {stderr}");
    assert!(stdout.starts_with("<!DOCTYPE html>"), "missing doctype");
    assert!(stdout.contains("<style>"), "css should be embedded");
    assert!(stdout.contains("country name maps to code"));
    assert!(stdout.contains("class=\"rule failed\""));
    assert!(stdout.contains("Ambiguous input"));
    assert!(stdout.contains("</html>"));

    // Outline with anchors and back links.
    assert!(stdout.contains("id=\"outline\""), "missing outline");
    assert!(stdout.contains("id=\"rule-1\""), "missing rule anchor");
    assert!(stdout.contains("href=\"#rule-4\""), "outline link missing");
    assert!(stdout.contains("href=\"#outline\""), "back link missing");

    // The outline marks how each rule's mapping was obtained.
    assert!(stdout.contains("class=\"map auto\""), "missing auto mapping badge");
    assert!(stdout.contains("class=\"map file\""), "missing file mapping badge");

    // The outline is sorted by target column name.
    let outline = {
        let start = stdout.find("id=\"outline\"").unwrap();
        let end = stdout[start..].find("</nav>").unwrap() + start;
        &stdout[start..end]
    };
    let positions: Vec<usize> = ["city_code", "country_code", "end_date", "ref_tags"]
        .iter()
        .map(|name| outline.find(name).unwrap())
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "outline is not sorted by target: {positions:?}"
    );
}

#[test]
fn html_escapes_hostile_values() {
    let dir = std::env::temp_dir().join(format!("fvalidate-html-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let csv_path = dir.join("evil.csv");
    std::fs::write(
        &csv_path,
        "id,a,b\n1,\"<script>alert(1)</script>\",\"x & y\"\n",
    )
    .unwrap();
    let rules_path = dir.join("evil.vl");
    std::fs::write(&rules_path, "rule \"escape\" {\n left = a\n right = b\n}\n").unwrap();

    let (stdout, _, ok) = run(&[
        csv_path.to_str().unwrap(),
        "-r",
        rules_path.to_str().unwrap(),
        "--id-column",
        "id",
        "--format",
        "html",
        "--no-fail",
    ]);
    assert!(ok);
    assert!(!stdout.contains("<script>alert"), "raw markup leaked");
    assert!(stdout.contains("&lt;script&gt;alert"));
    assert!(stdout.contains("x &amp; y"));

    std::fs::remove_dir_all(&dir).ok();
}

/// `trim` and an optional relation (`allow_empty`): empty source/target pairs
/// are skipped rather than failed.
#[test]
fn trim_and_optional_empty() {
    let report = run_json(&[
        &manifest("examples/members.csv"),
        "-r",
        &manifest("examples/rules_optional.vl"),
        "--id-column",
        "member_id",
        "--format",
        "json",
        "--no-fail",
    ]);

    assert_eq!(report["rows_checked"], 7);
    let rules = report["rules"].as_array().unwrap();

    // trim = true + allow_empty = true: padded values are recognized and the
    // empty/whitespace-only rows are skipped, so the rule passes.
    assert_eq!(rules[0]["status"], "passed");
    assert_eq!(rules[0]["rows_passed"], 5);
    assert_eq!(rules[0]["rows_failed"], 0);
    assert_eq!(rules[0]["rows_skipped"], 2);

    // Without trim the padded values are unmapped and fail; the truly empty
    // pair is still skipped by allow_empty.
    assert_eq!(rules[1]["rows_failed"], 2);
    assert_eq!(rules[1]["rows_skipped"], 1);
    assert_eq!(rules[1]["unmapped_values"], 2);
}

/// `or(...)` fallback columns, `validation_skipped` predicates and
/// `mapping_filter`.
#[test]
fn fallback_skip_and_mapping_filter() {
    let report = run_json(&[
        &manifest("examples/loose.csv"),
        "-r",
        &manifest("examples/rules_filters.vl"),
        "--id-column",
        "id",
        "--format",
        "json",
        "--no-fail",
    ]);

    assert_eq!(report["rows_checked"], 8);
    let rules = report["rules"].as_array().unwrap();

    // `or(a, b, c)` uses the first non-empty column; rows 5-8 disagree.
    assert_eq!(rules[0]["left"], "or(code_a, code_b, code_c)");
    assert_eq!(rules[0]["rows_passed"], 4);
    assert_eq!(rules[0]["rows_failed"], 4);
    assert_eq!(rules[0]["rows_skipped"], 0);

    // `validation_skipped = in(status, [...])` skips row 4 (archived).
    assert_eq!(rules[1]["rows_skipped"], 0);
    assert_eq!(rules[1]["rows_validation_skipped"], 1);
    assert_eq!(rules[1]["rows_failed"], 4);
    // The skipped row is grouped alongside the matching/failing examples.
    let skipped = rules[1]["validation_skipped_results"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["count"], 1);
    assert_eq!(skipped[0]["ids"].as_array().unwrap().len(), 1);
    // A rule with no validation skips reports an empty list, not a missing key.
    assert!(rules[0]["validation_skipped_results"]
        .as_array()
        .unwrap()
        .is_empty());

    // `validation_skipped = any_in([...], [...])` skips row 7 (code SKIP).
    assert_eq!(rules[2]["rows_skipped"], 0);
    assert_eq!(rules[2]["rows_validation_skipped"], 1);
    assert_eq!(rules[2]["rows_failed"], 3);

    // `mapping_filter = eq(category, "standard")` filters the *reference rows*:
    // LEGACY is excluded, so row 8 is unmapped and fails.
    assert_eq!(rules[3]["rows_skipped"], 0);
    assert_eq!(rules[3]["rows_failed"], 3);
    assert_eq!(rules[3]["unmapped_values"], 1);
    assert_eq!(rules[3]["mapping"]["distinct_inputs"], 6);

    // For `auto` the filter runs over the data rows, so the secondary `FR -> ZZ`
    // row does not make `FR` ambiguous — but it is still validated and fails.
    assert_eq!(rules[4]["mapping"]["distinct_inputs"], 7);
    assert_eq!(rules[4]["mapping"]["ambiguous_inputs"], 0);
    assert_eq!(rules[4]["rows_skipped"], 0);
    assert_eq!(rules[4]["rows_failed"], 1);
}

/// xan-style regex: `regex(...)` in transforms, a rule `pattern`, and a
/// regex separator.
#[test]
fn regex_constructs_work() {
    let report = run_json(&[
        &manifest("examples/contacts.csv"),
        "-r",
        &manifest("examples/rules_regex.vl"),
        "--id-column",
        "contact_id",
        "--format",
        "json",
        "--no-fail",
    ]);

    assert_eq!(report["rows_checked"], 5);
    let rules = report["rules"].as_array().unwrap();

    // `replace(regex("[^0-9]"), "")`
    assert_eq!(rules[0]["rows_passed"], 3);
    assert_eq!(rules[0]["rows_failed"], 2);

    // `pattern` + `compare = matches`, with no `right` column.
    assert_eq!(rules[1]["right"], "(pattern)");
    assert_eq!(rules[1]["rows_passed"], 4);
    assert_eq!(rules[1]["rows_failed"], 1);

    // `match(regex("^([^@]+)@"), 1)`; a missing match is a transform error.
    assert_eq!(rules[2]["rows_passed"], 4);
    assert_eq!(rules[2]["transform_errors"], 1);

    // `separator = regex("\\s*[;,|]\\s*")`
    assert_eq!(rules[3]["status"], "passed");
    assert_eq!(rules[3]["rows_passed"], 5);
}

/// Parallel and sequential runs must agree on every statistic; only the
/// per-row `row` number is unavailable in parallel mode.
#[test]
fn parallel_matches_sequential() {
    let dir = std::env::temp_dir().join(format!("fvalidate-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let csv_path = dir.join("data.csv");

    let mut csv = String::from(
        "id,country_name,country_code,start_date,end_date,tags,ref_tags,city,city_code\n",
    );
    for i in 1..=5000u32 {
        let country = if i % 3 == 0 { "France" } else { "USA" };
        let code = if i % 97 == 0 { "FR" } else { "US" };
        csv.push_str(&format!(
            "{i},{country},{code},2020-01-01,2020-01-0{},a;b,a;b,Paris,PAR\n",
            (i % 9) + 1
        ));
    }
    std::fs::write(&csv_path, csv).unwrap();

    let rules = people_rules();
    let csv_str = csv_path.to_str().unwrap();
    let sequential = run_json(&[
        csv_str,
        "-r",
        &rules,
        "--id-column",
        "id",
        "-j",
        "1",
        "--format",
        "json",
        "--no-fail",
    ]);
    let parallel = run_json(&[
        csv_str,
        "-r",
        &rules,
        "--id-column",
        "id",
        "-j",
        "4",
        "--format",
        "json",
        "--no-fail",
    ]);

    assert_eq!(strip_rows(sequential), strip_rows(parallel));

    std::fs::remove_dir_all(&dir).ok();
}

/// Without an id column, parallel validation still reports exact global row
/// numbers (via a per-segment counting pass), so it matches sequential fully.
#[test]
fn parallel_without_id_keeps_row_numbers() {
    let dir = std::env::temp_dir().join(format!("fvalidate-noid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let csv_path = dir.join("data.csv");

    let mut csv = String::from(
        "country_name,country_code,start_date,end_date,tags,ref_tags,city,city_code\n",
    );
    for i in 1..=3000u32 {
        let country = if i % 3 == 0 { "France" } else { "USA" };
        let code = if i % 97 == 0 { "FR" } else { "US" };
        csv.push_str(&format!(
            "{country},{code},2020-01-01,2020-01-0{},a;b,a;b,Paris,PAR\n",
            (i % 9) + 1
        ));
    }
    std::fs::write(&csv_path, csv).unwrap();

    let rules = people_rules();
    let csv_str = csv_path.to_str().unwrap();
    let sequential = run_json(&[
        csv_str,
        "-r",
        &rules,
        "-j",
        "1",
        "--format",
        "json",
        "--no-fail",
    ]);
    let parallel = run_json(&[
        csv_str,
        "-r",
        &rules,
        "-j",
        "4",
        "--format",
        "json",
        "--no-fail",
    ]);

    // Row numbers are exact in both modes, so compare the reports verbatim.
    assert_eq!(sequential, parallel);
    assert_eq!(parallel["rows_checked"], 3000);

    std::fs::remove_dir_all(&dir).ok();
}

fn strip_rows(mut report: Value) -> Value {
    if let Some(rules) = report["rules"].as_array_mut() {
        for rule in rules {
            for key in ["pass_results", "fail_results"] {
                if let Some(examples) = rule[key].as_array_mut() {
                    for example in examples {
                        example.as_object_mut().unwrap().remove("row");
                    }
                }
            }
            if let Some(ambiguities) = rule
                .get_mut("mapping")
                .and_then(|m| m.get_mut("ambiguities"))
                .and_then(|a| a.as_array_mut())
            {
                for ambiguity in ambiguities {
                    if let Some(examples) = ambiguity["examples"].as_array_mut() {
                        for example in examples {
                            example.as_object_mut().unwrap().remove("row");
                        }
                    }
                }
            }
        }
    }
    report
}

/// The GUI evaluates a rules file through the library and needs two things the
/// CLI report does not expose: the column indices a rule references (for the
/// "rule attributes only" mode) and every matching/failing row on demand (for
/// "show all rows"). This exercises both through `EngineConfig::collect_hits`.
#[test]
fn engine_collects_all_hits_and_rule_columns() {
    use fast_csv::engine::{self, EngineConfig};
    use fast_csv::{dsl, rules};

    let dir = std::env::temp_dir().join(format!("fvalidate-hits-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let csv_path = dir.join("data.csv");
    let rules_path = dir.join("rules.vl");

    std::fs::write(
        &csv_path,
        "id,country_name,country_code\n1,USA,US\n2,France,FR\n3,Spain,ES\n4,France,US\n",
    )
    .unwrap();
    std::fs::write(
        &rules_path,
        "rule \"country\" {\n  left = country_name\n  right = country_code\n  mapping = auto\n}\n",
    )
    .unwrap();

    let headers = engine::read_headers(&csv_path, b',').unwrap();
    let program = dsl::load_file(&rules_path).unwrap();
    let plan = rules::compile(program, &headers).unwrap();
    let config = EngineConfig {
        path: csv_path.clone(),
        delimiter: b',',
        threads: 1,
        id_idx: Some(0),
        progress: None,
        collect_hits: Some(0),
    };

    let report = engine::run(&plan, &config).unwrap();
    let rule = &report.rules[0];

    // The rule reads `country_name` (1) and `country_code` (2).
    assert_eq!(rule.rule_columns, vec![1, 2]);

    // Every checked row is retained, split between pass and fail.
    assert_eq!(rule.hits.len() as u64, rule.rows_checked);
    let passed = rule.hits.iter().filter(|hit| hit.passed).count() as u64;
    let failed = rule.hits.iter().filter(|hit| !hit.passed).count() as u64;
    assert_eq!(passed, rule.rows_passed);
    assert_eq!(failed, rule.rows_failed);

    // The ids line up with the failing rows (`France` is ambiguous: FR and US).
    let failed_ids: Vec<&str> = rule
        .hits
        .iter()
        .filter(|hit| !hit.passed)
        .map(|hit| hit.id.as_str())
        .collect();
    assert!(failed_ids.contains(&"4"));

    // Without `collect_hits` the list stays empty (the report is bounded).
    let plain = engine::run(
        &plan,
        &EngineConfig {
            collect_hits: None,
            ..config
        },
    )
    .unwrap();
    assert!(plain.rules[0].hits.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

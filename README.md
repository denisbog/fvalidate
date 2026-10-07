# fast-csv / `fvalidate`

A fast, dependency-light Rust command line tool that validates relationships
between columns of a **large CSV file**, driven by a small rule DSL.

It answers questions like:

* *Do `start_date` and `end_date` hold the same instant, even though one is
  `2020-02-01` and the other `01/02/2020`?*
* *Does every `country_name` map to the expected `country_code`?*
* *Do the comma-separated `tags` match `ref_tags` regardless of order?*
* *Does the reference table (`city → code`) agree with my data, and where is
  the reference itself ambiguous?*

## Why is `xan` so fast, and what did we borrow from it?

[`xan`](https://github.com/medialab/xan) is a CSV toolkit built by médialab on
top of their [`simd-csv`](https://github.com/medialab/simd-csv) crate. Its speed
comes from a handful of deliberate choices, all of which are applied here:

| xan technique | How it is used in `fvalidate` |
| --- | --- |
| **SIMD-accelerated CSV parsing** — `simd-csv` mixes a state machine with `memchr`-style vectorised string searching, with runtime AVX2 detection. | All parsing goes through `simd_csv::Reader`, so the same parser speed applies. |
| **Zero-copy / reused buffers** — records are read into a single pre-allocated `ByteRecord` that is cleared and refilled, avoiding per-row allocation. | The hot loops keep one `ByteRecord` plus reusable `String`/`Vec` scratch buffers for the whole file. |
| **Column indexing instead of name lookup** — headers are resolved once, columns accessed by integer index. | Rules are compiled to `left_idx`/`right_idx`, and a `Slots` table maps them to a per-row cell vector. |
| **Record-aligned parallel segments** — `Seeker` finds safe byte offsets between records, then rayon workers read each segment independently. | `engine::segments_for` uses `Seeker::segments`; pass 1 (mapping extraction) and pass 2 (validation) run `rayon` over those ranges. |
| **Streaming, bounded memory** — nothing keeps the whole file in RAM. | Ambiguity examples use a bounded min-hash reservoir; matching/failing rows are aggregated by condition and keep a bounded id sample per group. |
| **Tuned release profile** — LTO, single codegen unit, `opt-level = 3`. | See `[profile.release]` in `Cargo.toml`; build with `RUSTFLAGS='-C target-cpu=native'` for AVX2. |

Cold results on a 300k-row / 17 MB file with 4 rules:

```
sequential (-j 1): 0.48 s
parallel   (-j 4): 0.14 s
```

Both produce byte-identical reports (except for row numbers, which are only
meaningful in sequential mode).

## Build

```bash
cargo build --release
# optional: make SIMD use every feature of your CPU
RUSTFLAGS='-C target-cpu=native' cargo build --release
```

The binary is `target/release/fvalidate`.

## Usage

```bash
fvalidate <input.csv> -r <rules.vl> [options]
cat data.csv | fvalidate - -r rules.vl --id-column id
```

| Option | Description |
| --- | --- |
| `-r, --rules <FILE>` | Rule DSL file (required). |
| `-d, --delimiter <BYTE>` | Field delimiter; `\t`/`tab` accepted (default `,`). |
| `--id-column <NAME>` | Column holding a unique value used to identify rows. Without it, a lightweight parallel counting pass still assigns exact global row numbers, so validation stays parallel. |
| `-n, --examples <N>` | Number of most frequent result groups per rule, each with up to `N` example ids (default `10`; overridable per rule). |
| `-j, --threads <N>` | Worker threads (`0` = all cores). |
| `--format <text\|json\|html>` | Report format (default `text`). |
| `--title <TITLE>` | Title used by the HTML report. |
| `-o, --output <FILE>` | Write the report to a file. |
| `--no-fail` | Always exit `0`, even if rules fail. |
| `--no-progress` | Disable the progress bar. |

Exit code is `1` when at least one rule fails, `0` otherwise.

While a large file is being read, a progress bar is drawn on **stderr** (only
when stderr is a terminal, so piping the report is unaffected). It shows the
share of the file processed, throughput and ETA, then prints a one-line summary:

```
[=============================>]  89% 28.7 MB / 32.1 MB  54.2 MB/s  ETA 00:01
Processed 32.1 MB in 0.75s (42.5 MB/s)
```

Use `--no-progress` to turn it off.

## The rule DSL

A rule set is a `defaults` block plus any number of `rule` blocks. `#` starts a
comment (outside quotes).

```text
defaults {
  separator       = ";"
  multi           = false
  compare         = eq
  report_limit    = 10
  mapping_separator = ","
}

rule "start and end dates agree" {
  left            = start_date
  right           = end_date
  transform_left  = date(["%Y-%m-%d", "%d/%m/%Y"], "%Y-%m-%d")
  transform_right = date(["%Y-%m-%d", "%d/%m/%Y"], "%Y-%m-%d")
  compare         = eq
}

rule "country name maps to code" {
  left           = country_name
  right          = country_code
  transform_left = trim | lower      # pipeline
  mapping        = auto              # extracted from the data
}

rule "tags agree as sets" {
  left      = tags
  right     = ref_tags
  multi     = true
  separator = ";"
  compare   = eq
}

rule "city maps to code via reference files" {
  left          = city
  right         = city_code
  mapping_files = ["examples/citymap1.csv", "examples/citymap2.csv"]
  mapping_left  = city
  mapping_right = code
}
```

### Values

A value is a bare token, a quoted string (`"..."` or `'...'`), a list
`[...]`, a call `name(args...)`, or a pipeline `a | b | c`. Values may span
several lines as long as brackets/quotes balance.

### Rule keys

| Key | Meaning |
| --- | --- |
| `left`, `right` | Columns to compare: a header name, `#index`, a list `[a, b]` forming a composite key, or a fallback `or(a, b, c)` that picks the first non-empty column. |
| `transform_left`, `transform_right` | Normalization pipeline (see below). With several columns it is applied to **each** component before joining. |
| `derive` | Xan/moonblade-style expression(s) computing named values from the row, usable in `left` / `right` (see [Derived values](#derived-values-derive)). |
| `compare` | `eq` (default), `ne`, `subset`, `superset`, `intersect`, or regex `matches` / `not_matches`. Operates on token sets. |
| `multi` | Split cells into multiple values before comparing (default `false`). |
| `separator` | Token separator when `multi = true`; a plain string or `regex("...")`. |
| `join_separator` | Glue used to combine several columns of one side into a single key (default `|`). |
| `pattern` | Rule-level regex used by `compare = matches` / `not_matches`; `right` may then be omitted. |
| `trim` | Trim each extracted cell (and reference-file cell) before transforming (default `false`). |
| `allow_empty` (alias `optional`) | When `true`, a row whose source and target are both empty is **skipped** instead of failed (default `false`). |
| `validation_skipped` (aliases `skip_when`, `skip`) | A predicate; rows where it holds are counted as **validation skipped** (see [Row predicates](#row-predicates)). |
| `mapping_filter` | A predicate selecting which rows **define a mapping**. For `mapping = auto` it runs over the data rows; for `mapping_files` it runs over the reference rows (columns resolved against each file's header). It never skips validation. |
| `mapping` | `none` (default) or `auto` (extract from the data). |
| `mapping_files` | List of reference CSVs; enables file-based mapping. |
| `mapping_left`, `mapping_right` | Column(s) inside the reference files. `mapping_left` lists form a composite key; several `mapping_right` columns list acceptable targets (the row matches if its right side equals any of them). |
| `mapping_multi`, `mapping_separator` | Multi-value handling inside reference files. |
| `report_limit` | Overrides `-n` for this rule. |

Defaults can be set for `separator`, `multi`, `compare`, `report_limit`,
`mapping_separator`, `join_separator`, `trim` and `allow_empty`.

### Validating a target that depends on two input values

When the expected value is only determined by a **combination** of columns,
give `left` (or `right`) a list. The components are transformed individually,
then joined with `join_separator` into a single lookup key. This works for both
auto-extracted and reference-file mappings.

```text
# The warehouse can only be known from the pair (product, region).
rule "warehouse derived from product and region" {
  left            = [product, region]
  right           = warehouse
  transform_left  = trim | lower          # applied to product and to region
  transform_right = trim | upper
  mapping_files   = ["examples/warehouse_map.csv"]
  mapping_left    = [product, region]     # two key columns in the reference
  mapping_right   = warehouse
}
```

Run the bundled example:

```bash
fvalidate examples/orders.csv -r examples/rules_composite.vl --id-column order_id
```

It reports `product + region` as the key, flags the rows whose `warehouse`
does not match the pair, and — in the `auto` variant — reports `widget|eu` and
`gizmo|apac` as ambiguous composite keys.

### Derived values (`derive`)

Sometimes the value to compare does not exist as a column yet. `derive`
computes named extra values from the row using a compact xan/moonblade-style
expression language; the names then behave like columns in `left` / `right`
(and shadow input columns of the same name).

```text
# r_version_label looks like "2.0, CURRENT, APPROVED".
rule "attachment version is the X.Y token" {
  derive = 'r_version_label.match(/\d+\.\d+/) or "" as expected'
  left   = expected
  right  = attachment_version_for_submission__v
}

rule "status from the lifecycle state" {
  derive = 'if(r_current_state eq "2" and contains(upper(r_version_label), "EFFECTIVE"), "Effective", ["Draft","In Review","Approved","Superseded","Obsolete"][int(r_current_state)]) as expected'
  left   = expected
  right  = status__v
}
```

The language supports literals (including regexes `/.../`), column identifiers,
`or`/`and`/`not`, comparisons (`==`, `eq`, `ne`, `<`, `in`, …), arithmetic,
`++` concatenation, indexing/slicing, lists, pipelines, and calls such as
`match`, `split`, `replace`, `trim`, `lower`, `upper`, `contains`,
`startswith`, `endswith`, `int`, `float`, `string`, `coalesce`, `if`,
`in_any([a, b], [accepted...])`, and more. Each clause ends with `as <name>`
or `as (<name>, <name>)`; several clauses are comma-separated and evaluated in
order. Expressions are parsed once and evaluated per row.

### Cascading reference values

When the expected value can come from one of several reference columns, list
them in `mapping_right`. Every non-empty column is an **acceptable target**: a
row passes when its right side matches **any** of them. This also works when
`right` itself is a list of columns.

```text
# The right value may match the Veeva DocType, the Veeva Subtype or the
# Classification, whichever the reference row provides.
rule "classification__v from classification-mapping.csv" {
  derive          = 'if(is_template == "T", "T", "") as template_key'
  left            = [r_object_type, subtype_code, doc_subtype, category, template_key]
  right           = classification__v
  transform_right = trim
  mapping_files   = ["classification-mapping.csv"]
  mapping_left    = ["Bracco Object Type", "Bracco Subtype Code", "Bracco Subtype", "Bracco Category", "Template"]
  mapping_right   = ["Veeva DocType", "Veeva Subtype", "Classification"]
}
```

A single `mapping_right` column keeps the plain behaviour: the canonical
(most frequent) target is used and differing targets are reported as ambiguous.
With several `mapping_right` columns the targets are alternatives, so the key
is not reported as ambiguous.

### Transforms

Applied left-to-right, zero allocation after warm-up:

* `trim`, `collapse` (trim + squeeze whitespace)
* `lower`, `upper`
* `date(fmt)`, `date(fmt, out)`, `date([fmt1, fmt2], out)` — parses with the
  first matching format (`RFC 3339` as a last resort) and re-emits with `out`.
* `int`, `float`, `bool` — canonical numeric/boolean forms.
* `replace(from, to)`, `prefix(s)`, `suffix(s)`
* **Regex** (see [Regex expressions](#regex-expressions)):
  `replace(regex("p"), "r")`, `regex_replace("p", "r")`, `match(regex("p")[, group])`,
  `regex_keep(regex("p"))`.

### Regex expressions

Regex support mirrors xan: `regex("...")` compiles a pattern once (at rule
compile time) and is used by other expressions.

| Expression | xan equivalent | Behaviour |
| --- | --- | --- |
| `regex("p")` | `regex("p")` | A compiled pattern value. Bare as a transform it extracts the whole match. |
| `replace(regex("p"), "r")` | `replace(s, regex("p"), "r")` | Regex replacement with capture groups (`$1`, `${name}`). A plain string stays a literal replace. |
| `regex_replace("p", "r")` | — | Regex replacement where the pattern may be a plain string. |
| `match(regex("p")[, n])` | `match(s, regex("p"), n)` | Extracts capture group `n` (default `0`, the whole match). A missing match is a transform error (the value is left unchanged). |
| `regex_keep(regex("p"))` | — | Keeps only the concatenation of all matches (e.g. strip non-digits). |
| `separator = regex("p")` | `split(s, regex("p"))` | Splits multi-value cells on a regex. |
| `pattern` + `compare = matches` | `match(s, regex("p"))` as a filter | Validates the `left` value against a rule-level regex; `right` is optional. `not_matches` inverts it. |

```text
rule "phone digits" {
  left           = phone_raw
  right          = phone_digits
  transform_left = replace(regex("[^0-9]"), "")     # xan-style regex replace
}

rule "email shape" {
  left    = email
  pattern = "^[^@[:space:]]+@[^@[:space:]]+\\.[A-Za-z]{2,}$"
  compare = matches                                   # single-column validation
}

rule "tags (regex separator)" {
  left      = tags
  right     = ref_tags
  multi     = true
  separator = regex("\\s*[;,|]\\s*")                  # split on ; , or |
}
```

Run the bundled example:

```bash
fvalidate examples/contacts.csv -r examples/rules_regex.vl --id-column contact_id
```

### Comparison semantics

Every cell is normalized to a **set of tokens** (one token unless `multi`):

1. the `left` cell is transformed, then optionally mapped to expected target
   token(s);
2. the `right` cell is transformed into the actual token set;
3. `compare` is evaluated on the two sorted, de-duplicated sets.

Because sets are used, `multi = true` makes comparison order-independent
(`a;b == b;a`). When a side is a list of columns, the individual values are
normalized first and then joined with `join_separator` before this pipeline.

## Mapping

A mapping is the relation `left value → target value(s)`.

* **`mapping = auto`** extracts the relation from the data itself by observing
  `(left, right)` pairs. When a left value is associated with several targets it
  is **ambiguous**; the most frequent target becomes the canonical one used for
  validation, so minority rows fail and the ambiguity is reported. When `multi`
  is enabled, tokens are paired positionally (`left[i] ↔ right[i]`).
* **`mapping_files = [...]`** loads one or more reference tables (each with its
  own header row) and unions their relations. `mapping_left`/`mapping_right`
  may be lists, in which case the reference values form a composite key. The
  rule's transforms are applied to the reference values as well, so keys and
  targets are normalized exactly like the data. `Paris → PAR` in one file and
  `Paris → PAR2` in another is reported as an ambiguity. Any known target is
  accepted; unmatched left values fail and are counted as `unmapped_values`.
  A `mapping_filter` restricts which reference rows are loaded (see
  [Row predicates](#row-predicates)).

Reference files are parsed **once** and cached for the whole run. Rules that
ask for the same files with the same columns, transforms and filter share a
single in-memory relation, so a lookup table used by several rules is neither
re-read nor re-parsed (and identical requirements reuse the built mapping).

Pass 1 builds the auto mapping in parallel (per-segment local counters merged at
the end); pass 2 validates against it.

## Report

The report is available as human-readable text (`--format text`, default),
machine-readable JSON (`--format json`) or as a **self-contained HTML page**
(`--format html`, no external assets, light/dark aware, all values
HTML-escaped). The HTML report opens with an **outline** — one row per rule,
sorted by target column — showing the validation summary (status, checked,
passed, failed, skipped) and a **mapping badge** that highlights how the rule
resolves its values: `auto` (indigo, the relation is extracted from the data) or
`file` (amber, loaded from reference CSVs); click a rule name to jump to its
section, and use the “↑ outline” link there to return. For every rule the report
contains:

* the number of rows checked, passed, **skipped**, **validation skipped** and failed;
* transform errors and unmapped values;
* matching, failed and **validation-skipped** rows **aggregated by condition** —
  the `(left, right, expected)` values — with the row count and up to `N`
  example ids per condition, most frequent condition first (`-n` controls `N`,
  i.e. how many conditions and how many ids per condition are shown; ties are
  broken by value, so the report is deterministic);
* the **complete** extracted/loaded mapping;
* for every ambiguous input (up to `N`), the distinct target values with counts,
  and example rows.

Example (truncated):

```
====================================================================
 CSV validation report
====================================================================
Rules processed : 4
Rules passed    : 0
Rules failed    : 4
Rows checked    : 8

[2] "country name maps to code"  FAILED
    left  : country_name
    right : country_code
    checked=8 passed=7 failed=1 skipped=0 validation_skipped=0 transform_errors=0 unmapped_values=0
    matching results (4):
      count=4 left="france" right="FR" expected="FR" ids=["7", "1", "2", "8"]
      count=1 left="germany" right="DE" expected="DE" ids=["6"]
      ...
    failed results (1):
      count=1 left="france" right="US" expected="FR" ids=["5"]
    validation-skipped results (1):
      count=1 left="IT" right="IT" ids=["4"]
    mapping (auto): 4 distinct inputs, 1 ambiguous
      ambiguous input "france" -> "FR" (4), "US" (1)
        example: row=1 id="1" left="france" right="FR" expected="FR"
    full mapping:
      "france" -> "FR" [ambiguous]  {"FR" (4), "US" (1)}
      "germany" -> "DE"  {"DE" (1)}
      "united states" -> "US"  {"US" (1)}
      "usa" -> "US"  {"US" (1)}
```

A rule is marked `failed` when it has failing rows **or** an ambiguous mapping.

### Optional values and trimming

Real data often has optional fields. Two options make this pleasant:

* `trim = true` strips surrounding whitespace from every extracted cell and
  from reference-file values, so `" France "` is looked up as `"France"`.
* `allow_empty = true` makes a relation **optional**: when the source and the
  target are both empty (after trimming/transforms), the row is counted as
  `skipped` rather than `failed`. For `pattern` rules (no target), an empty
  value is skipped.

```text
rule "country code from reference (trim + optional)" {
  left          = country_name
  right         = country_code
  trim          = true
  allow_empty   = true
  mapping_files = ["examples/country_codes.csv"]
  mapping_left  = country_name
  mapping_right = country_code
}
```

Run the bundled example:

```bash
fvalidate examples/members.csv -r examples/rules_optional.vl --id-column member_id
```

### Row predicates

`validation_skipped` and `mapping_filter` take a small boolean expression over
a row's cells. It is evaluated on the raw cell value (trimmed when
`trim = true`), before any transform:

| Predicate | Meaning |
| --- | --- |
| `in(col, ["a", "b"])` | the value is one of the listed strings (alias `one_of`) |
| `any_in([a, b], ["x", "y"])` | at least one of the listed columns is one of the values |
| `all_in([a, b], [...])` | every listed column is one of the values |
| `eq(col, "v")` / `ne(col, "v")` | value equals / differs from `v` |
| `empty(col)` / `not_empty(col)` | the value is empty / non-empty |
| `and(...)`, `or(...)`, `not(...)` | combine predicates (aliases `all`, `any`) |
| `true`, `false` | literals |

`validation_skipped` generalizes `allow_empty`: whenever the predicate holds,
the row is counted separately under `validation_skipped` (it is not mixed into
the `skipped` count, which only holds `allow_empty` skips) regardless of the
source/target values.

`mapping_filter` instead selects which rows **define the mapping** — it never
skips validation, so a filtered-out row is still checked against the resulting
relation:

* with `mapping = auto` it runs over the **data rows**, so only matching rows
  are observed while the relation is extracted;
* with `mapping_files` it runs over the **reference rows**, so only matching
  lookup entries are loaded. This is the place to filter on extra reference
  columns such as a category or version.

```text
rule "only primary rows define the mapping" {
  left           = or(country_name, country_name_short, country_name_en)
  right          = country_code
  mapping        = auto
  mapping_filter = eq(row_kind, "primary")
}

# Only the "standard" rows of the reference file are loaded; a deprecated
# entry is ignored and reported as an unmapped value.
rule "standard reference rows only" {
  left           = country_name
  right          = country_code
  mapping_files  = ["examples/loose_map.csv"]
  mapping_left   = code
  mapping_right  = expected
  mapping_filter = eq(category, "standard")
}

rule "archived rows are not validated" {
  left               = country_name
  right              = country_code
  mapping_files      = ["examples/country_codes.csv"]
  mapping_left       = country_name
  mapping_right      = country_code
  validation_skipped = any_in([status, review_status], ["archived", "deleted"])
}
```

Run the bundled example:

```bash
fvalidate examples/loose.csv -r examples/rules_filters.vl --id-column id
```

## GUI viewer (`fview`, optional)

An optional [egui](https://github.com/emilk/egui) window — the same toolkit as
the PrintCraft shell — browses a big CSV and can evaluate a rules file against
the open file. It lives behind the `gui` feature, so the default `fvalidate`
build stays dependency-light:

```bash
cargo run --release --features gui --bin fview -- examples/orders.csv
```

The window has a filter/grep toolbar plus two dockable panels opened from the
toolbar: a **Config** panel on the left (scan modes, chip/table view,
hide/show attributes, saved profiles, theme) and a **Rules** panel on the
right:

* **Open rules…** picks a `.vl` file and evaluates it against the CSV that is
  currently open.
* The panel shows only the per-rule **statistics** (status and
  `checked / passed / failed / skipped / validation skipped`); the rows
  themselves are never listed there.
* Clicking the **rule name** redirects the **main grid** to every row of that
  rule (not just a sample), so the usual chip/table view and its
  attribute/profiler controls apply to those rows. Clicking the **passed**,
  **failed**, **skipped** or **validation skipped** count shows only that
  outcome; the active count turns into a **clear** action, and searching returns
  the grid to the normal scan.
* The **rule attributes only** checkbox hides every column the active rule does
  not read from the main grid (and from the row detail form), so a wide file
  collapses to just the attributes that matter; turning it off restores the
  previous attribute set.
* Every attribute chip (and each table header) carries a **lock** icon. A locked
  column is pinned visible: **Hide all**, the hide eye, saved profiles and
  "rule attributes only" all leave it in the grid. Click the filled lock to
  release it (unlocking keeps the column visible; hide it with the eye).
* A **rows** drop-down sets how many matching rows the grid keeps (100 by
  default, up to 10 000); changing it re-runs the search.

The evaluation is performed by the same `engine` the CLI uses, through the
crate's library target.

### Filtering non-empty values with the regex box

The filter is a regular expression tested as a **substring** against every
visible cell (case-insensitive unless `--case-sensitive`), and a row matches
when **any** searched cell matches. To find rows where one column is non-empty:

1. **Hide all** attributes, then click the eye on the column you want (the
   **Attributes** search finds it quickly).
2. Keep **visible only** checked so only that column is searched.
3. Filter with `.` (any character) for *non-empty*, or `\S` for *non-blank* (a
   cell of spaces does not match).

For several columns, reveal each of them and hide the rest, keeping **visible
only** on: a row matches when at least one revealed column is non-empty. The
cells are OR-ed, so one regex cannot require *all* of them to be non-empty; do
that one column at a time (or with a validation rule). Note that while a column
is **indexed** and the **index** checkbox is on, a non-empty filter is a
`beginsWith` prefix query rather than a regex.

## Generating fixture data (`fgen`)

`fgen` writes a deterministic CSV of city temperature records (with a matching
`city,country` reference table) for trying out the tool. A configurable fraction
of the rows is deliberately corrupted, so the file always mixes **matching** and
**non-matching** attributes across the bundled weather rules:

```bash
# 1000 rows, ~10% mismatched, plus the reference mapping.
cargo run --bin fgen -- 1000 -o examples/weather.csv --reference examples/weather_cities.csv

fvalidate examples/weather.csv -r examples/rules_weather.vl --id-column id
```

| Option | Meaning |
| --- | --- |
| `<ROWS>` | Number of data rows to generate (positional). |
| `-o, --output <FILE>` | Output CSV (default `weather.csv`). |
| `--seed <N>` | Deterministic seed; the same seed reproduces the same file. |
| `--bad-rate <0..1>` | Fraction of rows with a deliberate mismatch (default `0.1`). |
| `--start-date <YYYY-MM-DD>` | First record date (default `2024-01-01`). |
| `--days <N>` | Days the records are spread over (default `365`). |
| `--reference <FILE>` | Also write the `city,country` reference table. |

Each row has `id`, `city`, `country`, `recorded_on`, `temp_c`, `temp_f` and
`humidity_pct`. Non-matching rows cycle through three flaws: a swapped country,
a Fahrenheit value that disagrees with the Celsius reading, and a non-ISO date.

## Project layout

```
src/
  main.rs       CLI, stdin spooling, output
  lib.rs        library target shared by `fvalidate` and `fview`
  bin/fview.rs  optional GUI viewer + rule-evaluation panel (feature `gui`)
  bin/fgen.rs   deterministic temperature-record generator (fixture data)
  dsl.rs        DSL tokenizer/parser -> Program
  rules.rs      compilation of rules against headers (column indices)
  transform.rs  value transforms
  compare.rs    set comparison operators
  mapping.rs    mapping storage, reference-file loading, ambiguity
  sampler.rs    bounded distinct min-hash reservoir
  progress.rs   dependency-free stderr progress bar
  engine.rs     segment discovery + parallel passes + orchestration
  report.rs     report model and text/JSON rendering
tests/
  integration.rs
examples/
  people.csv    small fixture
  rules.vl      example rule set (single-column keys, transforms, multi, mappings)
  citymap1.csv, citymap2.csv
  orders.csv    composite-key fixture
  warehouse_map.csv
  rules_composite.vl   two-input-value example
  contacts.csv  regex fixture
  rules_regex.vl       regex example
  members.csv   trim / optional-relation fixture
  country_codes.csv
  rules_optional.vl    trim + allow_empty example
  loose.csv    fallback / skip / mapping_filter fixture
  loose_map.csv
  rules_filters.vl     or(...) + validation_skipped + mapping_filter example
  weather_cities.csv   city -> country reference used by `rules_weather.vl`
  rules_weather.vl     temperature-record rules (date, conversion, mapping)
```

## Tests

```bash
cargo test
```

Unit tests cover the DSL, transforms and sampler; integration tests run the
binary end-to-end (including stdin and parallel-equals-sequential) and exercise
the engine's GUI hooks (`collect_hits`, rule columns) through the library.

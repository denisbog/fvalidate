//! `fgen` — generate a deterministic CSV of city temperature records for
//! exercising `fvalidate`.
//!
//! Each row has a unique `id`, a city (with its country), the date the record
//! was taken and the temperature in both Celsius and Fahrenheit. A configurable
//! fraction of the rows is deliberately corrupted so the generated file always
//! contains a mix of **matching** and **non-matching** attributes:
//!
//! * a wrong `country` for the `city` (breaks the reference-mapping rule);
//! * a wrong `temp_f` for the recorded `temp_c` (breaks the conversion rule);
//! * a non-ISO `recorded_on` date (breaks the date-format rule).
//!
//! Run it with:
//!
//! ```text
//! cargo run --bin fgen -- 1000 -o examples/weather.csv --reference examples/weather_cities.csv
//! fvalidate examples/weather.csv -r examples/rules_weather.vl --id-column id
//! ```

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::{Days, NaiveDate};
use clap::Parser;

/// City, country and yearly-mean temperature (°C). Mirrored by
/// `examples/weather_cities.csv`, which the validation rules use as a
/// reference mapping.
const CITIES: [(&str, &str, f64); 10] = [
    ("Paris", "France", 12.0),
    ("Berlin", "Germany", 10.0),
    ("London", "United Kingdom", 11.5),
    ("Madrid", "Spain", 15.0),
    ("Rome", "Italy", 16.0),
    ("Amsterdam", "Netherlands", 10.5),
    ("Vienna", "Austria", 11.0),
    ("Lisbon", "Portugal", 17.0),
    ("Warsaw", "Poland", 9.0),
    ("Stockholm", "Sweden", 7.0),
];

/// Columns written by the generator.
const HEADER: &str = "id,city,country,recorded_on,temp_c,temp_f,humidity_pct";

#[derive(Parser, Debug)]
#[command(
    name = "fgen",
    version,
    about = "Generate a CSV of city temperature records (matching + non-matching) for fvalidate"
)]
struct Args {
    /// Number of data rows to generate.
    rows: usize,

    /// Output CSV file.
    #[arg(short, long, default_value = "weather.csv")]
    output: PathBuf,

    /// Seed for the deterministic generator (same seed -> same file).
    #[arg(long, default_value_t = 42)]
    seed: u64,

    /// Fraction of rows that carry a deliberate mismatch (0.0..=1.0).
    #[arg(long, default_value_t = 0.1)]
    bad_rate: f64,

    /// First record date (YYYY-MM-DD).
    #[arg(long, default_value = "2024-01-01")]
    start_date: String,

    /// Number of days the records are spread over (inclusive start).
    #[arg(long, default_value_t = 365)]
    days: u32,

    /// Also write the `city,country` reference table to this file.
    #[arg(long)]
    reference: Option<PathBuf>,
}

/// Small SplitMix64 generator: deterministic, dependency-free and more than
/// good enough for fixture data.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }
}

/// Which attribute is corrupted on a non-matching row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flaw {
    Country,
    Fahrenheit,
    Date,
}

#[derive(Debug, Clone, Copy, Default)]
struct Stats {
    matching: usize,
    non_matching: usize,
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Build the full CSV text and the matching / non-matching counts.
fn generate(
    rows: usize,
    seed: u64,
    bad_rate: f64,
    start: NaiveDate,
    days: u32,
) -> (String, Stats) {
    let mut rng = Rng::new(seed);
    let bad_rate = bad_rate.clamp(0.0, 1.0);

    // Pick the flawed rows up front, then guarantee a mix (when there is more
    // than one row) so the validation report always shows both outcomes.
    let mut flawed: Vec<bool> = (0..rows).map(|_| rng.next_f64() < bad_rate).collect();
    if rows >= 2 {
        if !flawed.iter().any(|&bad| bad) {
            flawed[rows / 2] = true;
        }
        if flawed.iter().all(|&bad| bad) {
            flawed[rows / 2] = false;
        }
    }

    let days = days.max(1);
    let mut out = String::new();
    out.push_str(HEADER);
    out.push('\n');

    let mut stats = Stats::default();
    let mut flaw_counter = 0usize;

    for (index, &is_bad) in flawed.iter().enumerate() {
        let city_index = rng.below(CITIES.len());
        let (city, country, base) = CITIES[city_index];
        let offset = rng.below(days as usize);
        let date = start + Days::new(offset as u64);

        // Seasonal swing around the city's mean, plus a little noise, so the
        // temperatures look plausible rather than uniform.
        let seasonal = 12.0 * ((offset as f64 / 365.0) * std::f64::consts::TAU).sin();
        let noise = (rng.next_f64() - 0.5) * 6.0;
        let temp_c = base + seasonal + noise;
        let temp_c_text = format!("{temp_c:.1}");

        // Derive Fahrenheit from the *printed* Celsius value, so the rules can
        // recompute it and get exactly the same number.
        let c: f64 = temp_c_text.parse().unwrap_or(temp_c);
        let temp_f = (c * 9.0 / 5.0 + 32.0).round() as i64;

        let humidity = 30 + rng.below(61) as i64;

        let mut country = country.to_string();
        let mut date_text = date.format("%Y-%m-%d").to_string();
        let mut temp_f = temp_f;

        if is_bad {
            let flaw = match flaw_counter % 3 {
                0 => Flaw::Country,
                1 => Flaw::Fahrenheit,
                _ => Flaw::Date,
            };
            match flaw {
                Flaw::Country => {
                    let other = (city_index + 1) % CITIES.len();
                    country = CITIES[other].1.to_string();
                }
                Flaw::Fahrenheit => temp_f += 3,
                Flaw::Date => date_text = date.format("%d/%m/%Y").to_string(),
            }
            flaw_counter += 1;
            stats.non_matching += 1;
        } else {
            stats.matching += 1;
        }

        let id = format!("W{:06}", index + 1);
        let _ = writeln!(
            out,
            "{id},{},{},{date_text},{temp_c_text},{temp_f},{humidity}",
            csv_field(city),
            csv_field(&country),
        );
    }

    (out, stats)
}

/// The `city,country` reference table used by the mapping rule.
fn reference_csv() -> String {
    let mut out = String::from("city,country\n");
    for (city, country, _) in CITIES {
        let _ = writeln!(out, "{city},{country}");
    }
    out
}

fn write_file(path: &PathBuf, contents: &str, what: &str) -> Result<(), String> {
    std::fs::write(path, contents)
        .map_err(|err| format!("cannot write {what} {}: {err}", path.display()))
}

fn main() -> ExitCode {
    let args = Args::parse();

    let start = match NaiveDate::parse_from_str(&args.start_date, "%Y-%m-%d") {
        Ok(date) => date,
        Err(_) => {
            eprintln!(
                "fgen: invalid --start-date {:?} (expected YYYY-MM-DD)",
                args.start_date
            );
            return ExitCode::FAILURE;
        }
    };

    let (csv, stats) = generate(args.rows, args.seed, args.bad_rate, start, args.days);

    if let Err(err) = write_file(&args.output, &csv, "data") {
        eprintln!("fgen: {err}");
        return ExitCode::FAILURE;
    }
    if let Some(path) = &args.reference {
        if let Err(err) = write_file(path, &reference_csv(), "reference") {
            eprintln!("fgen: {err}");
            return ExitCode::FAILURE;
        }
    }

    println!(
        "wrote {} rows to {} ({} matching, {} non-matching)",
        args.rows,
        args.output.display(),
        stats.matching,
        stats.non_matching
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start() -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()
    }

    #[test]
    fn writes_the_requested_number_of_rows() {
        let (csv, stats) = generate(50, 7, 0.2, start(), 365);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 51);
        assert_eq!(lines[0], HEADER);
        for line in &lines[1..] {
            assert_eq!(line.split(',').count(), 7, "row: {line}");
        }
        assert_eq!(stats.matching + stats.non_matching, 50);
    }

    #[test]
    fn always_mixes_matching_and_non_matching_rows() {
        for rate in [0.0, 0.5, 1.0] {
            let (_, stats) = generate(10, 1, rate, start(), 365);
            assert!(
                stats.matching > 0 && stats.non_matching > 0,
                "rate {rate}: {stats:?}"
            );
        }
    }

    #[test]
    fn is_deterministic_for_a_seed() {
        let a = generate(20, 99, 0.3, start(), 30).0;
        let b = generate(20, 99, 0.3, start(), 30).0;
        assert_eq!(a, b);
    }

    #[test]
    fn reference_lists_every_city() {
        let reference = reference_csv();
        for (city, country, _) in CITIES {
            assert!(reference.contains(&format!("{city},{country}")));
        }
    }
}

mod compare;
mod dsl;
mod engine;
mod expr;
mod mapping;
mod pattern;
mod progress;
mod report;
mod rules;
mod sampler;
mod transform;

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};

use crate::engine::EngineConfig;
use crate::rules::{ColumnResolver, Plan};

#[derive(Parser, Debug)]
#[command(
    name = "fvalidate",
    version,
    about = "Fast CSV column validation driven by a small rule DSL",
    long_about = "Validates relationships between two columns of a large CSV file.\n\n\
        Comparisons can be direct, normalized by transforms (e.g. date parsing) or\n\
        resolved through a value mapping extracted from the data or loaded from\n\
        reference files. The report lists, per rule, distinct matching and failing\n\
        rows, the complete mapping and ambiguous mappings."
)]
struct Cli {
    /// Input CSV file. Omit or use `-` to read from stdin.
    input: Option<PathBuf>,

    /// Path to the rules DSL file.
    #[arg(short = 'r', long)]
    rules: PathBuf,

    /// Field delimiter (single byte; use '\t' or 'tab' for tabs).
    #[arg(short = 'd', long, default_value = ",")]
    delimiter: String,

    /// Column holding a unique value, used to identify rows in the report.
    #[arg(long)]
    id_column: Option<String>,

    /// Number of example rows per rule (can be overridden per rule in the DSL).
    #[arg(short = 'n', long, default_value_t = 10)]
    examples: usize,

    /// Number of worker threads (0 = all available cores).
    #[arg(short = 'j', long, default_value_t = 0)]
    threads: usize,

    /// Report format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Title used by the HTML report (defaults to the report heading).
    #[arg(long)]
    title: Option<String>,

    /// Write the report to this file instead of stdout.
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// Always exit with code 0, even when some rules fail.
    #[arg(long)]
    no_fail: bool,

    /// Disable the progress bar (shown on stderr when it is a terminal).
    #[arg(long)]
    no_progress: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Json,
    Html,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(failed) => {
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let delimiter = parse_delimiter(&cli.delimiter)?;

    // Everything downstream works on a seekable file, so stdin is spooled.
    let spooled;
    let path: &Path = match cli.input.as_deref() {
        None => {
            spooled = spool_stdin()?;
            spooled.path()
        }
        Some(p) if p.as_os_str() == "-" => {
            spooled = spool_stdin()?;
            spooled.path()
        }
        Some(p) => p,
    };

    let headers = engine::read_headers(path, delimiter)?;

    let mut program = dsl::load_file(&cli.rules)?;
    program.defaults.report_limit = cli.examples;
    let plan: Plan = rules::compile(program, &headers)?;

    let id_idx = match &cli.id_column {
        Some(name) => Some(ColumnResolver::resolve(name, &headers).ok_or_else(|| {
            format!(
                "id column '{name}' not found (available: {})",
                headers.join(", ")
            )
        })?),
        None => None,
    };

    let threads = if cli.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        cli.threads
    };

    let progress = progress::Progress::new(!cli.no_progress && progress::stderr_is_terminal());

    let config = EngineConfig {
        path: path.to_path_buf(),
        delimiter,
        threads,
        id_idx,
        progress: Some(progress),
    };

    let report = engine::run(&plan, &config)?;

    let rendered = match cli.format {
        Format::Text => report.render_text(),
        Format::Json => report.render_json(),
        Format::Html => {
            let title = cli.title.clone().unwrap_or_else(|| {
                cli.input
                    .as_deref()
                    .filter(|p| p.as_os_str() != "-")
                    .map(|p| format!("CSV validation report — {}", p.display()))
                    .unwrap_or_else(|| "CSV validation report".to_string())
            });
            report.render_html(&title)
        }
    };

    match &cli.output {
        Some(output) => {
            let mut file = File::create(output)
                .map_err(|e| format!("cannot create {}: {e}", output.display()))?;
            file.write_all(rendered.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", output.display()))?;
        }
        None => {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(rendered.as_bytes())
                .map_err(|e| format!("cannot write to stdout: {e}"))?;
        }
    }

    Ok(report.rules_failed > 0 && !cli.no_fail)
}

fn parse_delimiter(raw: &str) -> Result<u8, String> {
    match raw {
        "\\t" | "tab" | "TAB" => Ok(b'\t'),
        "\\0" => Ok(0),
        other => {
            let bytes = other.as_bytes();
            if bytes.len() == 1 {
                Ok(bytes[0])
            } else {
                Err(format!(
                    "delimiter must be a single byte, got '{other}' (use '\\t' for a tab)"
                ))
            }
        }
    }
}

/// A temporary file that removes itself when dropped.
struct Spool {
    path: PathBuf,
}

impl Spool {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read all of stdin into a temporary file so it can be seeked and processed in
/// parallel.
fn spool_stdin() -> Result<Spool, String> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("fvalidate-{}-{unique}.csv", std::process::id()));

    let mut file = File::create(&path)
        .map_err(|e| format!("cannot create temporary file {}: {e}", path.display()))?;

    let mut stdin = io::stdin().lock();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = stdin
            .read(&mut buffer)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|e| format!("cannot spool stdin: {e}"))?;
    }
    file.flush()
        .map_err(|e| format!("cannot spool stdin: {e}"))?;

    Ok(Spool { path })
}

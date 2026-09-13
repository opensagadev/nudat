use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use nudat::{pack_with_progress, Archive, Entry, Format, PackPhase};
use rayon::ThreadPoolBuilder;
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    name = "nudat",
    version,
    about = "Inspect and edit Traveller's Tales Nu engine DAT archives"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Pc,
    Android,
    Obb,
}

impl From<OutputFormat> for Format {
    fn from(value: OutputFormat) -> Self {
        match value {
            OutputFormat::Pc => Self::Pc,
            OutputFormat::Android => Self::Android,
            OutputFormat::Obb => Self::Obb,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Show archive version and file count.
    Info { archive: PathBuf },
    /// List every file, optionally filtering by a substring.
    List {
        archive: PathBuf,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        long: bool,
    },
    /// Summarize directories without listing every file.
    Tree {
        archive: PathBuf,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        long: bool,
        /// Maximum directory depth to display.
        #[arg(long, default_value_t = 3)]
        depth: usize,
    },
    /// Write one decoded file to standard output.
    Cat { archive: PathBuf, path: String },
    /// Extract one file by its archive path.
    Extract {
        archive: PathBuf,
        path: String,
        output: PathBuf,
    },
    /// Extract all files into a directory.
    Unpack {
        archive: PathBuf,
        directory: PathBuf,
        /// Number of extraction workers (defaults to Rayon's thread count).
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Pack a directory as a new DAT archive.
    Pack {
        directory: PathBuf,
        output: PathBuf,
        /// Archive format (defaults to OBB for .obb output, otherwise PC).
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,
        /// Preserve compressed files from an original archive when rebuilding.
        #[arg(long, value_name = "ORIGINAL_ARCHIVE")]
        base: Option<PathBuf>,
        /// Number of packing workers (defaults to Rayon's thread count).
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Copy an archive with files replaced, added, or removed.
    Edit {
        archive: PathBuf,
        output: PathBuf,
        #[arg(long = "put", value_name = "ARCHIVE_PATH=DISK_FILE")]
        put: Vec<String>,
        #[arg(long = "remove", value_name = "ARCHIVE_PATH")]
        remove: Vec<String>,
    },
    /// Decode and check every entry.
    Verify { archive: PathBuf },
}

#[derive(Default)]
struct TreeNode<'a> {
    dirs: BTreeMap<String, (&'a str, TreeNode<'a>)>,
    files: BTreeMap<String, &'a Entry>,
    file_count: usize,
    stored_bytes: u64,
    decoded_bytes: u64,
}

struct TransferUiState {
    last_render: Instant,
    bytes_seen: u64,
    files_seen: usize,
    next_report: u64,
    last_rendered_bytes: u64,
}

struct TransferProgress {
    bar: ProgressBar,
    state: Mutex<TransferUiState>,
    interactive: bool,
    label: &'static str,
}

impl TransferProgress {
    fn new(label: &'static str, total_bytes: u64) -> Result<Self> {
        let bar = ProgressBar::new(total_bytes);
        bar.set_draw_target(ProgressDrawTarget::stderr_with_hz(10));
        bar.set_style(ProgressStyle::with_template(
            "{spinner:.green} {bytes}/{total_bytes} [{bar:30.cyan/blue}] {msg}",
        )?);
        Ok(Self {
            bar,
            state: Mutex::new(TransferUiState {
                last_render: Instant::now() - Duration::from_millis(100),
                bytes_seen: 0,
                files_seen: 0,
                next_report: 10,
                last_rendered_bytes: 0,
            }),
            interactive: io::stderr().is_terminal(),
            label,
        })
    }

    fn report(
        &self,
        path: &str,
        completed: usize,
        bytes: u64,
        total_files: usize,
        total_bytes: u64,
    ) {
        let mut state = self.state.lock().expect("progress state poisoned");
        let bytes = bytes.max(state.bytes_seen);
        let completed = completed.max(state.files_seen);
        let newly_complete = completed == total_files && state.files_seen < total_files;
        if state.last_render.elapsed() >= Duration::from_millis(100)
            || bytes.saturating_sub(state.last_rendered_bytes) >= 1024 * 1024
            || newly_complete
        {
            self.bar.set_length(total_bytes);
            self.bar.set_position(bytes);
            let path = if path.chars().count() > 48 {
                format!(
                    "…{}",
                    path.chars()
                        .rev()
                        .take(47)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect::<String>()
                )
            } else {
                path.to_owned()
            };
            self.bar
                .set_message(format!("{completed}/{total_files} {path}"));
            state.last_render = Instant::now();
            state.last_rendered_bytes = bytes;
        }
        state.bytes_seen = bytes;
        state.files_seen = completed;
        if !self.interactive {
            let percent = bytes
                .saturating_mul(100)
                .checked_div(total_bytes)
                .unwrap_or_else(|| completed as u64 * 100 / total_files.max(1) as u64);
            if percent >= state.next_report || newly_complete {
                let _ = writeln!(
                    io::stderr().lock(),
                    "{}: {percent}% ({completed}/{total_files} files, {bytes}/{total_bytes} bytes)",
                    self.label
                );
                state.next_report = (percent / 10 + 1) * 10;
            }
        }
    }

    fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

impl<'a> TreeNode<'a> {
    fn add(&mut self, entry: &'a Entry) {
        let mut node = self;
        node.include(entry);
        let mut parts = entry.path.split('\\').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                node = &mut node
                    .dirs
                    .entry(part.to_ascii_uppercase())
                    .or_insert_with(|| (part, TreeNode::default()))
                    .1;
                node.include(entry);
            } else {
                node.files.insert(part.to_ascii_uppercase(), entry);
            }
        }
    }

    fn include(&mut self, entry: &Entry) {
        self.file_count += 1;
        self.stored_bytes += entry.stored_size as u64;
        self.decoded_bytes += entry.size as u64;
    }
}

fn matches_filter(entry: &Entry, filter: &Option<String>) -> bool {
    filter.as_ref().is_none_or(|filter| {
        entry
            .path
            .to_ascii_lowercase()
            .contains(&filter.to_ascii_lowercase())
    })
}

fn print_list(out: &mut impl Write, node: &TreeNode<'_>, long: bool) -> io::Result<()> {
    for (_, child) in node.dirs.values() {
        print_list(out, child, long)?;
    }
    for entry in node.files.values() {
        if long {
            writeln!(
                out,
                "{:>10} {:>10} {:?} {}",
                entry.size, entry.stored_size, entry.compression, entry.path
            )?;
        } else {
            writeln!(out, "{}", entry.path)?;
        }
    }
    Ok(())
}

fn format_size(size: u64) -> String {
    if size < 1024 {
        return format!("{size} B");
    }
    let mut value = size as f64;
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB"] {
        value /= 1024.0;
        unit = next;
        if value < 1024.0 {
            break;
        }
    }
    format!("{value:.1} {unit}")
}

fn summary(node: &TreeNode<'_>, long: bool) -> String {
    let count = format!(
        "{} {}",
        node.file_count,
        if node.file_count == 1 {
            "file"
        } else {
            "files"
        }
    );
    if long {
        format!(
            "{count}, {} stored, {} decoded",
            format_size(node.stored_bytes),
            format_size(node.decoded_bytes)
        )
    } else {
        count
    }
}

fn print_tree(
    out: &mut impl Write,
    tree: &TreeNode<'_>,
    prefix: &str,
    level: usize,
    depth: usize,
    long: bool,
) -> io::Result<()> {
    if level >= depth {
        return Ok(());
    }
    for (index, (name, child)) in tree.dirs.values().enumerate() {
        let last = index + 1 == tree.dirs.len();
        writeln!(
            out,
            "{prefix}{} {name}/ ({})",
            if last { "└──" } else { "├──" },
            summary(child, long)
        )?;
        let next = format!("{prefix}{}", if last { "    " } else { "│   " });
        print_tree(out, child, &next, level + 1, depth, long)?;
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        if error.chain().any(|cause| {
            cause
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
        }) {
            return;
        }
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    match cli.command {
        Command::Info { archive } => {
            let dat = Archive::open(&archive)
                .with_context(|| format!("opening {}", archive.display()))?;
            writeln!(out, "version: {}", dat.version())?;
            writeln!(out, "files: {}", dat.entries().len())?;
            writeln!(
                out,
                "stored bytes: {}",
                dat.entries()
                    .iter()
                    .map(|e| e.stored_size as u64)
                    .sum::<u64>()
            )?;
            writeln!(
                out,
                "decoded bytes: {}",
                dat.entries().iter().map(|e| e.size as u64).sum::<u64>()
            )?;
        }
        Command::List {
            archive,
            filter,
            long,
        } => {
            let dat = Archive::open(&archive)
                .with_context(|| format!("opening {}", archive.display()))?;
            let mut tree = TreeNode::default();
            for entry in dat
                .entries()
                .iter()
                .filter(|entry| matches_filter(entry, &filter))
            {
                tree.add(entry);
            }
            print_list(&mut out, &tree, long)?;
        }
        Command::Tree {
            archive,
            filter,
            long,
            depth,
        } => {
            let dat = Archive::open(&archive)
                .with_context(|| format!("opening {}", archive.display()))?;
            let mut tree = TreeNode::default();
            for entry in dat
                .entries()
                .iter()
                .filter(|entry| matches_filter(entry, &filter))
            {
                tree.add(entry);
            }
            writeln!(
                out,
                "{} ({})",
                archive.file_name().unwrap_or_default().to_string_lossy(),
                summary(&tree, long)
            )?;
            print_tree(&mut out, &tree, "", 0, depth, long)?;
        }
        Command::Cat { archive, path } => {
            let dat = Archive::open(&archive)
                .with_context(|| format!("opening {}", archive.display()))?;
            dat.copy_to(&path, &mut out)
                .with_context(|| format!("reading {path}"))?;
        }
        Command::Extract {
            archive,
            path,
            output,
        } => {
            Archive::open(&archive)
                .with_context(|| format!("opening {}", archive.display()))?
                .extract(&path, &output)
                .with_context(|| format!("extracting {path}"))?;
        }
        Command::Unpack {
            archive,
            directory,
            jobs,
        } => {
            if jobs == Some(0) {
                bail!("--jobs must be at least 1");
            }
            let dat = Archive::open(&archive)
                .with_context(|| format!("opening {}", archive.display()))?;
            let total_files = dat.entries().len();
            let total_bytes = dat.entries().iter().map(|entry| entry.size as u64).sum();
            let progress = TransferProgress::new("unpack", total_bytes)?;
            let update = |entry: &Entry, completed: usize, bytes: u64| {
                progress.report(&entry.path, completed, bytes, total_files, total_bytes);
            };
            let result = if let Some(jobs) = jobs {
                let pool = ThreadPoolBuilder::new().num_threads(jobs).build()?;
                pool.install(|| dat.unpack_parallel_with_progress(&directory, update))
            } else {
                dat.unpack_parallel_with_progress(&directory, update)
            };
            progress.finish();
            result.with_context(|| format!("unpacking {}", archive.display()))?;
            writeln!(out, "unpacked {} files", dat.entries().len())?;
        }
        Command::Pack {
            directory,
            output,
            format,
            base,
            jobs,
        } => {
            if jobs == Some(0) {
                bail!("--jobs must be at least 1");
            }
            let inferred_format = format.unwrap_or_else(|| {
                if output
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("obb"))
                {
                    OutputFormat::Obb
                } else {
                    OutputFormat::Pc
                }
            });
            let original = base
                .as_ref()
                .map(Archive::open)
                .transpose()
                .with_context(|| {
                    format!(
                        "opening base archive {}",
                        base.as_ref().unwrap_or(&output).display()
                    )
                })?;
            if let Some(original) = &original {
                if format.is_some() && original.format() != Some(inferred_format.into()) {
                    bail!("--format does not match the base archive");
                }
                if output
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("obb"))
                    && original.format() != Some(Format::Obb)
                {
                    bail!("an .obb output requires an OBB base archive");
                }
            }
            let compare_progress = TransferProgress::new("compare", 0)?;
            let write_progress = TransferProgress::new("pack", 0)?;
            let update = |phase: PackPhase,
                          path: &str,
                          completed: usize,
                          bytes: u64,
                          total_files: usize,
                          total_bytes: u64| {
                match phase {
                    PackPhase::Compare => {
                        compare_progress.report(path, completed, bytes, total_files, total_bytes)
                    }
                    PackPhase::Write => {
                        write_progress.report(path, completed, bytes, total_files, total_bytes)
                    }
                }
            };
            let run = || {
                if let Some(original) = &original {
                    original.repack_with_progress(&directory, &output, update)
                } else {
                    pack_with_progress(&directory, &output, inferred_format.into(), update)
                }
            };
            let result = if let Some(jobs) = jobs {
                let pool = ThreadPoolBuilder::new().num_threads(jobs).build()?;
                pool.install(run)
            } else {
                run()
            };
            compare_progress.finish();
            write_progress.finish();
            result.with_context(|| format!("packing {}", directory.display()))?;
            writeln!(
                out,
                "packed {} files",
                Archive::open(&output)?.entries().len()
            )?;
        }
        Command::Edit {
            archive,
            output,
            put,
            remove,
        } => {
            if put.is_empty() && remove.is_empty() {
                bail!("specify at least one --put or --remove");
            }
            let replacements = put
                .iter()
                .map(|item| {
                    let (name, file) = item
                        .split_once('=')
                        .context("--put must be ARCHIVE_PATH=DISK_FILE")?;
                    Ok((name.to_owned(), PathBuf::from(file)))
                })
                .collect::<Result<Vec<_>>>()?;
            let dat = Archive::open(&archive)
                .with_context(|| format!("opening {}", archive.display()))?;
            dat.rewrite(&output, &replacements, &remove)
                .with_context(|| format!("writing {}", output.display()))?;
            writeln!(
                out,
                "wrote {} files",
                Archive::open(&output)?.entries().len()
            )?;
        }
        Command::Verify { archive } => {
            let dat = Archive::open(&archive)
                .with_context(|| format!("opening {}", archive.display()))?;
            dat.verify()
                .with_context(|| format!("verifying {}", archive.display()))?;
            writeln!(out, "verified {} files", dat.entries().len())?;
        }
    }
    out.flush()?;
    Ok(())
}

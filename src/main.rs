use clap::Parser;
use rayon::prelude::*;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use walkdir::WalkDir;

/// A multithreaded directory size tool (like du -sh)
#[derive(Parser)]
#[command(
    name = "dst",
    about = "Directory size tool — like `du -sh`",
    long_about = "Directory size tool — like `du -sh`\n\nExamples:\n  dst /some/dir              Show total size\n  dst /some/dir/*            List all items with sizes\n  dst /some/dir/*.rs         List matching items\n  dst /some/dir/* -m 1M      Only show items >= 1M\n  dst /some/dir/* -e test    Regex filter on path names\n\nSize suffixes: B, K/KB, M/MB, G/GB, T/TB (case-insensitive)"
)]
struct Args {
    /// Paths to analyze (use shell globs: dst /dir/*)
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Include hidden files and directories (starting with '.')
    #[arg(short, long)]
    all: bool,

    /// Sort by name instead of size
    #[arg(short = 'n', long)]
    sort_name: bool,

    /// Minimum size cutoff (e.g. 1K, 10M, 1G, 500MB)
    #[arg(short = 'm', long = "min")]
    min_size: Option<String>,

    /// Regex filter on path names
    #[arg(short = 'e', long = "regex")]
    filter: Option<String>,
}

/// Parse a human size string like "1G", "10MB", "20M", "1K", "500B" into bytes.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let pos = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_str, suffix) = s.split_at(pos);
    let num: f64 = num_str
        .parse()
        .map_err(|_| format!("invalid number in size: '{}'", s))?;
    let multiplier: u64 = match suffix.to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" => 1024,
        "M" | "MB" => 1024 * 1024,
        "G" | "GB" => 1024 * 1024 * 1024,
        "T" | "TB" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(format!("unknown size suffix: '{}'", suffix)),
    };
    Ok((num * multiplier as f64) as u64)
}

fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if bytes >= TB {
        format!("{:.1}T", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1}G", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}K", bytes as f64 / KB as f64)
    } else {
        format!("{}B", bytes)
    }
}

fn dir_size(path: &Path, include_hidden: bool) -> u64 {
    let total = Arc::new(AtomicU64::new(0));
    let entries: Vec<_> = WalkDir::new(path)
        .into_iter()
        .filter_entry(|e| include_hidden || e.depth() == 0 || !is_hidden(e))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .collect();
    entries.par_iter().for_each(|entry| {
        if let Ok(meta) = entry.metadata() {
            total.fetch_add(meta.len(), Ordering::Relaxed);
        }
    });
    total.load(Ordering::Relaxed)
}

fn main() {
    let args = Args::parse();

    let min_bytes = match &args.min_size {
        Some(s) => match parse_size(s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        },
        None => 0,
    };

    let re = match &args.filter {
        Some(pattern) => match Regex::new(pattern) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("error: invalid regex: {}", e);
                std::process::exit(1);
            }
        },
        None => None,
    };

    // Filter paths: skip hidden unless -a, apply regex filter
    let paths: Vec<PathBuf> = args
        .paths
        .iter()
        .filter(|p| {
            if !args.all {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with('.') {
                        return false;
                    }
                }
            }
            if let Some(re) = &re {
                return re.is_match(&p.display().to_string());
            }
            true
        })
        .cloned()
        .collect();

    // Single directory with no glob expansion — show total size
    if paths.len() == 1 && paths[0].is_dir() {
        let size = dir_size(&paths[0], args.all);
        if size >= min_bytes {
            println!("{}\t{}", format_size(size), paths[0].display());
        }
        return;
    }

    // Multiple paths (shell glob expanded) — size each item
    let mut entries: Vec<(PathBuf, u64)> = paths
        .into_par_iter()
        .map(|p| {
            let size = if p.is_dir() {
                dir_size(&p, args.all)
            } else {
                p.metadata().map(|m| m.len()).unwrap_or(0)
            };
            (p, size)
        })
        .collect();

    // Apply minimum size filter
    if min_bytes > 0 {
        entries.retain(|(_, size)| *size >= min_bytes);
    }

    if entries.is_empty() {
        eprintln!("no matching items");
        return;
    }

    // Sort
    if args.sort_name {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
    } else {
        entries.sort_by(|a, b| b.1.cmp(&a.1));
    }

    // Print
    let size_width = entries
        .iter()
        .map(|(_, s)| format_size(*s).len())
        .max()
        .unwrap_or(0);
    for (path, size) in &entries {
        println!(
            "{:>width$}  {}",
            format_size(*size),
            path.display(),
            width = size_width,
        );
    }
}

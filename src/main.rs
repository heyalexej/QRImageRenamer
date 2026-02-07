use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::io::Write;
use std::thread::JoinHandle;
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use clap::{Parser, ValueEnum};
use minijinja::{context, Environment, UndefinedBehavior};
use nom_exif::{Exif, ExifIter, ExifTag, MediaParser, MediaSource};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

#[derive(Clone, Copy, Debug, ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum OrderMode {
    Auto,
    Filename,
    Exif,
}

#[derive(Clone, Copy, Debug, ValueEnum, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ExtMode {
    /// Keep the original file extension (no dot), as-is.
    Keep,
    /// Lowercase the extension.
    Lower,
    /// Uppercase the extension.
    Upper,
    /// Use `ext_fixed` from config/CLI.
    Fixed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct FileConfig {
    template: Option<String>,
    padding: Option<usize>,
    start_index: Option<u8>, // 0 or 1
    unmatched_dir: Option<String>,

    order: Option<OrderMode>,
    exif_format: Option<String>,
    use_exif_for_order_fallback: Option<bool>,

    ext_mode: Option<ExtMode>,
    ext_fixed: Option<String>,
}

#[derive(Debug, Clone)]
struct Config {
    template: String,
    padding: usize,
    start_index: u8,
    unmatched_dir: String,

    order: OrderMode,
    exif_format: String,
    use_exif_for_order_fallback: bool,

    ext_mode: ExtMode,
    ext_fixed: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            template: "{{ qrcode_safe }}_1{{ n_pad }}.{{ ext_upper }}".to_string(),
            padding: 2,
            start_index: 1,
            unmatched_dir: "unmatched".to_string(),

            order: OrderMode::Auto,
            exif_format: "%Y-%m-%d_%H%M%S".to_string(),
            use_exif_for_order_fallback: true,

            ext_mode: ExtMode::Keep,
            ext_fixed: None,
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "qrir")]
#[command(about = "Scan images for QR codes and rename/copy them using a MiniJinja template.")]
struct Cli {
    #[arg(long)]
    input: Option<PathBuf>,

    #[arg(long)]
    output: Option<PathBuf>,

    /// Override the template string from config.
    #[arg(long)]
    template: Option<String>,

    /// Optional config path (TOML). Default: `~/.qr_image_renamer.toml` if present.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Extract EXIF timestamp and expose it as `exif_date` (also used for ordering fallback in `--order auto`).
    #[arg(long)]
    exif: bool,

    /// Ordering strategy.
    #[arg(long, value_enum)]
    order: Option<OrderMode>,

    /// Format string for `exif_date` (chrono/strftime style).
    #[arg(long)]
    exif_format: Option<String>,

    #[arg(long)]
    unmatched_dir: Option<String>,

    #[arg(long)]
    padding: Option<usize>,

    /// `n` is 0- or 1-based. Both `n0` and `n1` are always available.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=1))]
    start_index: Option<u8>,

    #[arg(long, value_enum)]
    ext_mode: Option<ExtMode>,

    #[arg(long)]
    ext_fixed: Option<String>,

    /// Number of threads (0 = rayon default).
    #[arg(long, default_value_t = 0)]
    threads: usize,

    /// Print progress (images/s) while running.
    #[arg(long)]
    progress: bool,

    /// Generate `html_inspect.json` and `html_inspect.html` in `--output` (gallery grouped by QR/SKU).
    #[arg(long)]
    html_inspect: bool,

    /// Print planned actions, do not write files.
    #[arg(long)]
    dry_run: bool,

    /// Move files instead of copying.
    #[arg(long)]
    r#move: bool,

    /// Overwrite existing destination files on disk. (Duplicates within the same run still error.)
    #[arg(long)]
    overwrite: bool,

    /// Print a default config TOML and exit.
    #[arg(long)]
    print_default_config: bool,
}

#[derive(Debug, Clone, Serialize)]
struct HtmlInspectImage {
    sku: String,
    sku_raw: String,
    src_path: String,
    src_name: String,
    dest_rel: String,
    dest_name: String,
}

#[derive(Debug, Clone, Serialize)]
struct HtmlInspectSku {
    sku: String,
    sku_raw: String,
    images: Vec<HtmlInspectImage>,
}

#[derive(Debug, Clone, Serialize)]
struct HtmlInspectData {
    generated_at: String,
    input_root: String,
    output_root: String,
    skus: Vec<HtmlInspectSku>,
}

struct ProgressState {
    total: usize,
    processed: AtomicUsize,
    start: std::time::Instant,
    done: AtomicBool,
}

struct ProgressGuard {
    state: Arc<ProgressState>,
    join: Option<JoinHandle<()>>,
}

impl ProgressGuard {
    fn start(total: usize) -> Self {
        let state = Arc::new(ProgressState {
            total,
            processed: AtomicUsize::new(0),
            start: std::time::Instant::now(),
            done: AtomicBool::new(false),
        });

        let st = Arc::clone(&state);
        let join = std::thread::spawn(move || {
            let mut last = 0usize;
            let mut last_t = std::time::Instant::now();

            // Print at most once per second.
            while !st.done.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(1000));

                let now = std::time::Instant::now();
                let cur = st.processed.load(Ordering::Relaxed);

                let dt = now.duration_since(last_t).as_secs_f64().max(1e-9);
                let inst_ips = (cur.saturating_sub(last)) as f64 / dt;
                let avg_ips = cur as f64 / st.start.elapsed().as_secs_f64().max(1e-9);

                let pct = (cur as f64) * 100.0 / (st.total as f64);
                eprint!(
                    "\rprogress: {cur}/{total} ({pct:.1}%) | {inst_ips:.1} img/s (inst) | {avg_ips:.1} img/s (avg)",
                    total = st.total
                );
                let _ = std::io::stderr().flush();

                last = cur;
                last_t = now;
            }

            // Newline to not leave a partial progress line behind.
            eprintln!();
        });

        Self {
            state,
            join: Some(join),
        }
    }

    fn inc(&self) {
        self.state.processed.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        self.state.done.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn default_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".qr_image_renamer.toml"))
}

fn load_file_config(path: &Path) -> Result<FileConfig> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let cfg: FileConfig = toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?;
    Ok(cfg)
}

fn merge_config(base: Config, fc: Option<FileConfig>, cli: &Cli) -> Result<Config> {
    let mut cfg = base;
    if let Some(fc) = fc {
        if let Some(v) = fc.template {
            cfg.template = v;
        }
        if let Some(v) = fc.padding {
            cfg.padding = v;
        }
        if let Some(v) = fc.start_index {
            cfg.start_index = v.min(1);
        }
        if let Some(v) = fc.unmatched_dir {
            cfg.unmatched_dir = v;
        }
        if let Some(v) = fc.order {
            cfg.order = v;
        }
        if let Some(v) = fc.exif_format {
            cfg.exif_format = v;
        }
        if let Some(v) = fc.use_exif_for_order_fallback {
            cfg.use_exif_for_order_fallback = v;
        }
        if let Some(v) = fc.ext_mode {
            cfg.ext_mode = v;
        }
        if let Some(v) = fc.ext_fixed {
            cfg.ext_fixed = Some(v);
        }
    }

    if let Some(v) = &cli.template {
        cfg.template = v.clone();
    }
    if let Some(v) = cli.padding {
        cfg.padding = v;
    }
    if let Some(v) = cli.start_index {
        cfg.start_index = v;
    }
    if let Some(v) = &cli.unmatched_dir {
        cfg.unmatched_dir = v.clone();
    }
    if let Some(v) = cli.order {
        cfg.order = v;
    }
    if let Some(v) = &cli.exif_format {
        cfg.exif_format = v.clone();
    }
    if let Some(v) = cli.ext_mode {
        cfg.ext_mode = v;
    }
    if let Some(v) = &cli.ext_fixed {
        cfg.ext_fixed = Some(v.clone());
    }

    if cfg.ext_mode == ExtMode::Fixed && cfg.ext_fixed.as_deref().unwrap_or("").is_empty() {
        anyhow::bail!("ext_mode=fixed requires ext_fixed to be set");
    }

    Ok(cfg)
}

fn is_image_path(p: &Path) -> bool {
    let Some(ext) = p.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg")
}

fn os_str_to_string_lossy(s: &OsStr) -> String {
    s.to_string_lossy().to_string()
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn sanitize_filename_segment(raw: &str) -> String {
    // Cross-platform conservative: no slashes, no control chars, no trailing dots/spaces.
    let mut out = String::with_capacity(raw.len());
    let mut prev_us = false;
    for ch in raw.chars() {
        let mut keep = ch;
        let ok = keep.is_ascii_alphanumeric() || matches!(keep, '-' | '_' | '.' | '+' | '@');
        if !ok {
            if ch.is_whitespace() {
                keep = '_';
            } else {
                keep = '_';
            }
        }
        if keep == '_' {
            if prev_us {
                continue;
            }
            prev_us = true;
        } else {
            prev_us = false;
        }
        out.push(keep);
    }

    // Avoid hidden-dot prefixes and Windows trailing dot/space rules.
    while out.starts_with('.') {
        out.remove(0);
    }
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        return "QR".to_string();
    }

    // Windows reserved device names (case-insensitive), prevent pathological cases.
    let upper = out.to_ascii_uppercase();
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3",
        "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if reserved.contains(&upper.as_str()) {
        return format!("_{out}");
    }

    out
}

fn ext_vars(ext_raw: &str, mode: ExtMode, fixed: Option<&str>) -> (String, String, String) {
    let ext = match mode {
        ExtMode::Keep => ext_raw.to_string(),
        ExtMode::Lower => ext_raw.to_ascii_lowercase(),
        ExtMode::Upper => ext_raw.to_ascii_uppercase(),
        ExtMode::Fixed => fixed.unwrap_or(ext_raw).to_string(),
    };
    let ext_lower = ext.to_ascii_lowercase();
    let ext_upper = ext.to_ascii_uppercase();
    (ext, ext_lower, ext_upper)
}

fn parse_trailing_digits(stem: &str) -> Option<(&str, &str)> {
    let bytes = stem.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == bytes.len() {
        return None;
    }
    Some((&stem[..i], &stem[i..]))
}

fn choose_numeric_group(stems: &[String]) -> Option<(String, usize)> {
    // Heuristic: pick the most common (prefix, digit_len) that covers >= 80% of files.
    let mut counts: HashMap<(String, usize), usize> = HashMap::new();
    for stem in stems {
        let Some((prefix, digits)) = parse_trailing_digits(stem) else {
            continue;
        };
        let key = (prefix.to_string(), digits.len());
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut best: Option<((String, usize), usize)> = None;
    for (k, c) in counts {
        best = match best {
            None => Some((k, c)),
            Some((bk, bc)) => {
                if c > bc {
                    Some((k, c))
                } else if c == bc && k.1 > bk.1 {
                    Some((k, c))
                } else {
                    Some((bk, bc))
                }
            }
        };
    }
    let ((prefix, digit_len), c) = best?;
    if c * 10 < stems.len() * 8 {
        return None;
    }
    Some((prefix, digit_len))
}

fn rel_dir(input_root: &Path, parent: &Path) -> PathBuf {
    match parent.strip_prefix(input_root) {
        Ok(p) => p.to_path_buf(),
        Err(_) => PathBuf::new(),
    }
}

fn validate_relpath(s: &str) -> Result<PathBuf> {
    if s.trim().is_empty() {
        anyhow::bail!("template produced empty path");
    }
    let p = PathBuf::from(s);
    if p.is_absolute() {
        anyhow::bail!("template produced an absolute path, which is not allowed: {s}");
    }
    for comp in p.components() {
        if matches!(comp, Component::ParentDir) {
            anyhow::bail!("template produced a path with '..', which is not allowed: {s}");
        }
    }
    Ok(p)
}

fn path_to_slash_string(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn escape_json_for_html_script_tag(json: &str) -> String {
    // Prevent `</script>` injection and keep HTML parsing stable.
    // This is the same approach many frameworks use when embedding JSON into HTML.
    let mut out = String::with_capacity(json.len());
    for ch in json.chars() {
        match ch {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(ch),
        }
    }
    out
}

fn decode_qr_zxing(path: &Path) -> Result<Option<String>> {
    use zxingcpp::{BarcodeFormat, ImageFormat, ImageView};

    let img = image::open(path).with_context(|| format!("open image {}", path.display()))?;
    let gray = img.to_luma8();
    let (w, h) = gray.dimensions();
    let iv = ImageView::from_slice(gray.as_raw(), w, h, ImageFormat::Lum)?;

    let reader = zxingcpp::read()
        .formats(BarcodeFormat::QRCode)
        .try_rotate(true)
        .try_downscale(true)
        .try_invert(false)
        .try_harder(false);

    match reader.from(&iv) {
        Ok(bcs) => {
            for bc in bcs {
                if bc.is_valid() {
                    return Ok(Some(bc.text()));
                }
            }
            Ok(None)
        }
        Err(_) => Ok(None),
    }
}

fn exif_dt_for_path(parser: &mut MediaParser, path: &Path) -> Option<NaiveDateTime> {
    let ms = MediaSource::file_path(path).ok()?;
    if !ms.has_exif() {
        return None;
    }
    let iter: ExifIter = parser.parse(ms).ok()?;
    let exif: Exif = iter.into();
    for tag in [ExifTag::DateTimeOriginal, ExifTag::CreateDate, ExifTag::ModifyDate] {
        let v = exif.get(tag)?;
        let (ndt, _offset) = v.as_time_components()?;
        return Some(ndt);
    }
    None
}

#[derive(Debug, Clone)]
struct Item {
    path: PathBuf,
    file_name: String,
    stem: String,
    ext_raw: String,
    parent: PathBuf,
    parent_rel: PathBuf,
    mtime: Option<SystemTime>,
    exif_dt: Option<NaiveDateTime>,
    qr: Option<String>,
}

#[derive(Debug, Clone)]
struct Action {
    src: PathBuf,
    dest: PathBuf,
    is_move: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.print_default_config {
        let cfg = Config::default();
        let fc = FileConfig {
            template: Some(cfg.template),
            padding: Some(cfg.padding),
            start_index: Some(cfg.start_index),
            unmatched_dir: Some(cfg.unmatched_dir),
            order: Some(cfg.order),
            exif_format: Some(cfg.exif_format),
            use_exif_for_order_fallback: Some(cfg.use_exif_for_order_fallback),
            ext_mode: Some(cfg.ext_mode),
            ext_fixed: cfg.ext_fixed,
        };
        let s = toml::to_string_pretty(&fc).context("serialize default config")?;
        print!("{s}");
        return Ok(());
    }

    let input_arg = cli.input.clone().context("--input is required (unless --print-default-config)")?;
    let output_arg = cli.output.clone().context("--output is required (unless --print-default-config)")?;

    if cli.threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(cli.threads)
            .build_global()
            .ok();
    }

    let config_path = match &cli.config {
        Some(p) => Some(p.clone()),
        None => default_config_path().filter(|p| p.exists()),
    };
    let file_cfg = match &config_path {
        Some(p) => Some(load_file_config(p)?),
        None => None,
    };
    let cfg = merge_config(Config::default(), file_cfg, &cli)?;

    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::SemiStrict);
    env.add_template("rename", &cfg.template)
        .context("compile template")?;
    let tpl = env.get_template("rename").context("get template")?;

    let input_root = input_arg
        .canonicalize()
        .with_context(|| format!("canonicalize input {}", input_arg.display()))?;
    std::fs::create_dir_all(&output_arg).with_context(|| format!("create output dir {}", output_arg.display()))?;
    let output_root = output_arg
        .canonicalize()
        .with_context(|| format!("canonicalize output {}", output_arg.display()))?;

    let mut groups: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for ent in WalkDir::new(&input_root).follow_links(false) {
        let ent = ent?;
        if !ent.file_type().is_file() {
            continue;
        }
        let p = ent.into_path();
        if !is_image_path(&p) {
            continue;
        }
        let parent = p.parent().unwrap_or(&input_root).to_path_buf();
        groups.entry(parent).or_default().push(p);
    }
    if groups.is_empty() {
        anyhow::bail!("no images found under {}", input_root.display());
    }

    let total_candidates = groups.values().map(|v| v.len()).sum::<usize>();
    let progress = if cli.progress {
        Some(ProgressGuard::start(total_candidates))
    } else {
        None
    };

    let mut all_actions: Vec<Action> = Vec::new();
    let mut inspect_images: Vec<HtmlInspectImage> = Vec::new();
    let mut total_images = 0usize;
    let mut total_qr_frames = 0usize;
    let mut total_unmatched = 0usize;
    let mut total_matched = 0usize;

    let mut group_dirs: Vec<PathBuf> = groups.keys().cloned().collect();
    group_dirs.sort_by(|a, b| natord::compare(&a.to_string_lossy(), &b.to_string_lossy()));

    for dir in group_dirs {
        let mut paths = groups.remove(&dir).unwrap_or_default();
        paths.sort_by(|a, b| natord::compare(&a.file_name().unwrap().to_string_lossy(), &b.file_name().unwrap().to_string_lossy()));

        let parent_rel = rel_dir(&input_root, &dir);

        // Build items.
        let mut items: Vec<Item> = paths
            .into_iter()
            .filter_map(|p| {
                let file_name = p.file_name().map(os_str_to_string_lossy)?;
                let stem = p.file_stem().map(os_str_to_string_lossy).unwrap_or_else(|| file_name.clone());
                let ext_raw = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
                Some(Item {
                    mtime: file_mtime(&p),
                    path: p,
                    file_name,
                    stem,
                    ext_raw,
                    parent: dir.clone(),
                    parent_rel: parent_rel.clone(),
                    exif_dt: None,
                    qr: None,
                })
            })
            .collect();

        if items.is_empty() {
            continue;
        }

        // EXIF extraction (optional; used for ordering fallback + template var).
        if cli.exif {
            items
                .par_iter_mut()
                .map_init(MediaParser::new, |parser, it| {
                    it.exif_dt = exif_dt_for_path(parser, &it.path);
                })
                .count();
        }

        // Decide ordering.
        let order = cli.order.unwrap_or(cfg.order);
        match order {
            OrderMode::Filename => {
                items.sort_by(|a, b| natord::compare(&a.file_name, &b.file_name));
            }
            OrderMode::Exif => {
                // Prefer EXIF timestamps when present; otherwise fall back to mtime; then filename.
                items.sort_by(|a, b| match (a.exif_dt, b.exif_dt) {
                    (Some(x), Some(y)) => x.cmp(&y).then_with(|| natord::compare(&a.file_name, &b.file_name)),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => match (a.mtime, b.mtime) {
                        (Some(x), Some(y)) => x.cmp(&y).then_with(|| natord::compare(&a.file_name, &b.file_name)),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => natord::compare(&a.file_name, &b.file_name),
                    },
                });
            }
            OrderMode::Auto => {
                let stems: Vec<String> = items.iter().map(|i| i.stem.clone()).collect();
                if let Some((prefix, digit_len)) = choose_numeric_group(&stems) {
                    items.sort_by(|a, b| {
                        let a_num = parse_trailing_digits(&a.stem)
                            .and_then(|(p, d)| (p == prefix && d.len() == digit_len).then(|| d.parse::<u64>().ok()).flatten());
                        let b_num = parse_trailing_digits(&b.stem)
                            .and_then(|(p, d)| (p == prefix && d.len() == digit_len).then(|| d.parse::<u64>().ok()).flatten());
                        match (a_num, b_num) {
                            (Some(x), Some(y)) => x.cmp(&y).then_with(|| natord::compare(&a.file_name, &b.file_name)),
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (None, None) => natord::compare(&a.file_name, &b.file_name),
                        }
                    });
                } else if cli.exif && cfg.use_exif_for_order_fallback {
                    items.sort_by(|a, b| match (a.exif_dt, b.exif_dt) {
                        (Some(x), Some(y)) => x.cmp(&y).then_with(|| natord::compare(&a.file_name, &b.file_name)),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => natord::compare(&a.file_name, &b.file_name),
                    });
                } else {
                    items.sort_by(|a, b| natord::compare(&a.file_name, &b.file_name));
                }
            }
        }

        // QR decode (always; QR-only).
        items
            .par_iter_mut()
            .map(|it| -> Result<()> {
                it.qr = decode_qr_zxing(&it.path)?;
                if let Some(p) = &progress {
                    p.inc();
                }
                Ok(())
            })
            .collect::<Result<Vec<_>>>()?;

        // Plan renames.
        let mut current_qr: Option<String> = None;
        let mut counter: u64 = 0;
        for (seq_index, it) in items.iter().enumerate() {
            total_images += 1;

            let is_qr_frame = it.qr.is_some();
            if let Some(qr) = &it.qr {
                total_qr_frames += 1;
                current_qr = Some(qr.clone());
                counter = 0;
            }

            let dest_rel = if current_qr.is_none() {
                total_unmatched += 1;
                // If the input root itself is the sequence dir, `parent_rel` is empty; include
                // the dir name anyway so unmatched photos are easier to browse.
                let rel = if it.parent_rel.as_os_str().is_empty() {
                    PathBuf::from(
                        it.parent
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "input".to_string()),
                    )
                } else {
                    it.parent_rel.clone()
                };
                PathBuf::from(&cfg.unmatched_dir).join(rel).join(&it.file_name)
            } else {
                total_matched += 1;
                let qrcode_raw = current_qr.as_deref().unwrap_or("");
                let qrcode_safe = sanitize_filename_segment(qrcode_raw);

                let n0 = counter;
                let n1 = counter + 1;
                let n = if cfg.start_index == 0 { n0 } else { n1 };
                let n_pad = format!("{:0width$}", n, width = cfg.padding);

                let (ext, ext_lower, ext_upper) = ext_vars(&it.ext_raw, cfg.ext_mode, cfg.ext_fixed.as_deref());

                let exif_date = it
                    .exif_dt
                    .map(|dt| dt.format(&cfg.exif_format).to_string())
                    .unwrap_or_default();

                let rendered = tpl.render(context! {
                    // QR vars
                    qrcode_raw => qrcode_raw,
                    qrcode_safe => qrcode_safe.clone(),
                    qrcode => qrcode_safe,
                    // counter vars
                    n0 => n0,
                    n1 => n1,
                    n => n,
                    n_pad => n_pad,
                    // file vars
                    ext => ext.clone(),
                    ending => ext,
                    ext_lower => ext_lower,
                    ext_upper => ext_upper,
                    orig_name => it.file_name.clone(),
                    orig_stem => it.stem.clone(),
                    // exif vars
                    exif_date => exif_date,
                    // source vars
                    src_rel_dir => it.parent_rel.to_string_lossy().to_string(),
                    src_dir_name => it.parent.file_name().map(os_str_to_string_lossy).unwrap_or_default(),
                    // control vars
                    is_qr_frame => is_qr_frame,
                    seq_index => seq_index,
                    seq_total => items.len(),
                })?;

                validate_relpath(rendered.trim())?
            };

            let dest_abs = output_root.join(&dest_rel);
            all_actions.push(Action {
                src: it.path.clone(),
                dest: dest_abs,
                is_move: cli.r#move,
            });

            if cli.html_inspect {
                if let Some(raw) = &current_qr {
                    let sku_raw = raw.clone();
                    let sku = sanitize_filename_segment(&sku_raw);
                    inspect_images.push(HtmlInspectImage {
                        sku: sku.clone(),
                        sku_raw,
                        src_path: it.path.to_string_lossy().to_string(),
                        src_name: it.file_name.clone(),
                        dest_rel: path_to_slash_string(&dest_rel),
                        dest_name: dest_rel
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    });
                }
            }

            if current_qr.is_some() {
                counter += 1;
            }
        }
    }

    // Duplicate destination detection within this run (always an error).
    let mut seen: HashMap<PathBuf, PathBuf> = HashMap::new();
    for a in &all_actions {
        if let Some(prev) = seen.insert(a.dest.clone(), a.src.clone()) {
            anyhow::bail!(
                "Two inputs map to the same destination:\n  {}\n  {}\n-> {}",
                prev.display(),
                a.src.display(),
                a.dest.display()
            );
        }
    }

    // Execute.
    let mut wrote = 0usize;
    let mut skipped_existing = 0usize;
    for a in &all_actions {
        if a.dest.exists() {
            if cli.overwrite {
                std::fs::remove_file(&a.dest).with_context(|| format!("remove existing {}", a.dest.display()))?;
            } else {
                skipped_existing += 1;
                continue;
            }
        }
        if cli.dry_run {
            println!("DRY: {} -> {}", a.src.display(), a.dest.display());
            continue;
        }
        if let Some(parent) = a.dest.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create dir {}", parent.display()))?;
        }
        if a.is_move {
            if let Err(e) = std::fs::rename(&a.src, &a.dest) {
                // Cross-device moves fail on many systems; fallback to copy+remove.
                // If this also fails, bubble the original context plus the copy error context.
                let copy_res = std::fs::copy(&a.src, &a.dest)
                    .with_context(|| format!("copy (after rename failed: {e}) {} -> {}", a.src.display(), a.dest.display()));
                copy_res?;
                std::fs::remove_file(&a.src).with_context(|| format!("remove {}", a.src.display()))?;
            }
        } else {
            std::fs::copy(&a.src, &a.dest).with_context(|| format!("copy {} -> {}", a.src.display(), a.dest.display()))?;
        }
        wrote += 1;
    }

    println!("images_total: {total_images}");
    println!("qr_frames: {total_qr_frames}");
    println!("matched: {total_matched}");
    println!("unmatched: {total_unmatched}");
    println!("planned_actions: {}", all_actions.len());
    if cli.dry_run {
        println!("dry_run: true");
    } else {
        println!("wrote: {wrote}");
        if skipped_existing > 0 {
            println!("skipped_existing: {skipped_existing} (use --overwrite to replace)");
        }
    }

    if cli.html_inspect {
        // Group images by SKU (sanitized), keep stable ordering by dest_rel.
        let mut by_sku: HashMap<String, HtmlInspectSku> = HashMap::new();
        for img in inspect_images {
            let entry = by_sku.entry(img.sku.clone()).or_insert_with(|| HtmlInspectSku {
                sku: img.sku.clone(),
                sku_raw: img.sku_raw.clone(),
                images: Vec::new(),
            });
            entry.images.push(img);
        }
        let mut skus: Vec<HtmlInspectSku> = by_sku.into_values().collect();
        for sku in &mut skus {
            sku.images.sort_by(|a, b| natord::compare(&a.dest_rel, &b.dest_rel));
        }
        skus.sort_by(|a, b| natord::compare(&a.sku, &b.sku));

        let data = HtmlInspectData {
            generated_at: chrono::Utc::now().to_rfc3339(),
            input_root: input_root.to_string_lossy().to_string(),
            output_root: output_root.to_string_lossy().to_string(),
            skus,
        };

        let json = serde_json::to_string_pretty(&data).context("serialize html inspect json")?;
        let json_path = output_root.join("html_inspect.json");
        std::fs::write(&json_path, &json).with_context(|| format!("write {}", json_path.display()))?;

        // Embed JSON in the HTML to avoid file:// fetch/CORS issues.
        let html_path = output_root.join("html_inspect.html");
        let embedded_json = escape_json_for_html_script_tag(&json);
        let html = format!(
            r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>QR Image Renamer Inspect</title>
  <style>
    :root {{
      color-scheme: light;
      --bg: #0b0c10;
      --panel: #141824;
      --text: #eef2ff;
      --muted: #aab2c5;
      --accent: #5eead4;
      --border: #262c3e;
    }}
    body {{
      margin: 0;
      font: 14px/1.35 ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial;
      background: radial-gradient(1200px 800px at 20% 0%, #172044 0%, var(--bg) 55%) fixed;
      color: var(--text);
    }}
    header {{
      position: sticky;
      top: 0;
      backdrop-filter: blur(8px);
      background: color-mix(in srgb, var(--bg) 72%, transparent);
      border-bottom: 1px solid var(--border);
      padding: 14px 18px;
      display: flex;
      gap: 12px;
      align-items: center;
      z-index: 5;
    }}
    header h1 {{
      font-size: 14px;
      margin: 0;
      letter-spacing: 0.02em;
      color: var(--muted);
      font-weight: 600;
      white-space: nowrap;
    }}
    header input {{
      flex: 1;
      min-width: 180px;
      padding: 10px 12px;
      border-radius: 10px;
      border: 1px solid var(--border);
      background: color-mix(in srgb, var(--panel) 92%, black);
      color: var(--text);
      outline: none;
    }}
    header .meta {{
      color: var(--muted);
      font-size: 12px;
      white-space: nowrap;
    }}
    main {{
      padding: 18px;
      max-width: 1400px;
      margin: 0 auto;
    }}
    .sku {{
      margin: 18px 0 26px;
      border: 1px solid var(--border);
      background: color-mix(in srgb, var(--panel) 88%, black);
      border-radius: 14px;
      overflow: hidden;
    }}
    .sku h2 {{
      margin: 0;
      padding: 14px 16px;
      font-size: 18px;
      letter-spacing: 0.01em;
      display: flex;
      gap: 10px;
      align-items: baseline;
      border-bottom: 1px solid var(--border);
    }}
    .sku h2 code {{
      font-size: 12px;
      color: var(--muted);
      font-weight: 500;
    }}
    .grid {{
      display: grid;
      gap: 12px;
      padding: 14px;
      grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    }}
    figure {{
      margin: 0;
      border: 1px solid var(--border);
      background: #0f1320;
      border-radius: 12px;
      overflow: hidden;
    }}
    figure a {{
      display: block;
      text-decoration: none;
      color: inherit;
    }}
    figure img {{
      width: 100%;
      height: 180px;
      object-fit: cover;
      display: block;
      background: #05060a;
    }}
    figcaption {{
      padding: 10px 10px 12px;
      font-size: 12px;
      color: var(--muted);
      border-top: 1px solid var(--border);
      display: grid;
      gap: 4px;
    }}
    .fname {{
      color: var(--text);
      font-weight: 600;
      word-break: break-word;
    }}
    .path {{
      word-break: break-word;
      opacity: 0.9;
    }}
    .empty {{
      color: var(--muted);
      padding: 22px;
      border: 1px dashed var(--border);
      border-radius: 14px;
      background: color-mix(in srgb, var(--panel) 60%, black);
    }}
  </style>
</head>
<body>
  <header>
    <h1>html_inspect</h1>
    <input id="q" placeholder="Filter by SKU..." autocomplete="off" />
    <div class="meta" id="meta"></div>
  </header>
  <main id="app"></main>

  <script id="data" type="application/json">{json}</script>
  <script>
    const data = JSON.parse(document.getElementById('data').textContent);
    const app = document.getElementById('app');
    const q = document.getElementById('q');
    const meta = document.getElementById('meta');

    function render() {{
      const needle = (q.value || '').trim().toLowerCase();
      app.innerHTML = '';

      const skus = data.skus.filter(s => !needle || s.sku.toLowerCase().includes(needle) || (s.sku_raw || '').toLowerCase().includes(needle));
      meta.textContent = `${{skus.length}} SKU(s), ${{data.skus.reduce((a,s)=>a+s.images.length,0)}} image(s)`;

      if (!skus.length) {{
        const div = document.createElement('div');
        div.className = 'empty';
        div.textContent = 'No results.';
        app.appendChild(div);
        return;
      }}

      for (const sku of skus) {{
        const wrap = document.createElement('section');
        wrap.className = 'sku';

        const h2 = document.createElement('h2');
        h2.textContent = sku.sku;
        const code = document.createElement('code');
        code.textContent = sku.sku_raw && sku.sku_raw !== sku.sku ? `raw: ${{sku.sku_raw}}` : `${{sku.images.length}} image(s)`;
        h2.appendChild(code);
        wrap.appendChild(h2);

        const grid = document.createElement('div');
        grid.className = 'grid';
        for (const img of sku.images) {{
          const fig = document.createElement('figure');
          const a = document.createElement('a');
          a.href = img.dest_rel;
          a.target = '_blank';
          a.rel = 'noreferrer';
          const im = document.createElement('img');
          im.loading = 'lazy';
          im.src = img.dest_rel;
          im.alt = img.dest_name || img.src_name;
          a.appendChild(im);
          fig.appendChild(a);

          const cap = document.createElement('figcaption');
          const f1 = document.createElement('div');
          f1.className = 'fname';
          f1.textContent = img.dest_name || img.src_name;
          const f2 = document.createElement('div');
          f2.className = 'path';
          f2.textContent = img.dest_rel;
          cap.appendChild(f1);
          cap.appendChild(f2);
          fig.appendChild(cap);

          grid.appendChild(fig);
        }}
        wrap.appendChild(grid);
        app.appendChild(wrap);
      }}
    }}

    q.addEventListener('input', render);
    render();
  </script>
</body>
</html>
"#,
            json = embedded_json
        );
        std::fs::write(&html_path, html).with_context(|| format!("write {}", html_path.display()))?;

        println!("html_inspect_json: {}", json_path.display());
        println!("html_inspect_html: {}", html_path.display());
    }

    Ok(())
}

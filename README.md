# qrir

Rename camera photos by scanning QR codes inside the images (JPEG only).

How it works:
- You point it at an input folder and an output folder.
- It scans images in order (per folder).
- When it sees a QR code (your SKU), that SKU becomes “active”.
- Every image after that is renamed using your template, until the next QR code appears.
- Images before the first QR code go into `unmatched/` (original filename kept).

This is a rewrite of a python tool I've built years ago. It has saved us weeks of our lives in production, processing hundreds of thousands of images.

## Install

Prebuilt binaries are intended for:
- macOS (x86_64, same as my olldie but goldie dev machine)
- Linux (x86_64)

This install method expects GitHub Release assets to exist (see `.github/workflows/release.yml`).

Install with curl:

```sh
curl -fsSL https://raw.githubusercontent.com/heyalexej/QRImageRenamer/main/scripts/install.sh | sh -s -- --repo heyalexej/QRImageRenamer
```

If you want a specific version:

```sh
curl -fsSL https://raw.githubusercontent.com/heyalexej/QRImageRenamer/main/scripts/install.sh | sh -s -- --repo heyalexej/QRImageRenamer --version v0.1.2
```

## Quick Start

Dry-run first:

```sh
qrir --input ~/Pictures/img_today --output ~/Pictures/output --dry-run
```

Then run for real (copy mode):

```sh
qrir --input ~/Pictures/img_today --output ~/Pictures/output
```

## Real-World Examples (Input -> Output)

Assume:
- Source folder: `~/Pictures/img_today`
- Target folder: `~/Pictures/output`
- Your camera files are named:
  - `DSC0986.JPG`
  - `DSC0987.JPG`
  - `DSC0988.JPG`
- Detected QR code (SKU) is: `555555`
- Default settings are:
  - `padding = 2`
  - `start_index = 1`

### Template A (Repo Default)

Template:
```jinja2
{{ qrcode_safe }}_1{{ n_pad }}.{{ ext_upper }}
```

Output:
```text
~/Pictures/img_today/DSC0986.JPG -> ~/Pictures/output/555555_101.JPG
~/Pictures/img_today/DSC0987.JPG -> ~/Pictures/output/555555_102.JPG
~/Pictures/img_today/DSC0988.JPG -> ~/Pictures/output/555555_103.JPG
```

Note: If you run `qrir` on multiple subfolders that reuse the same SKU, filenames can collide. In that case, use a template that includes `src_dir_name` or `src_rel_dir`.

### Template B (Put Each SKU In Its Own Folder)

Template:
```jinja2
{{ qrcode_safe }}/{{ qrcode_safe }}_{{ n_pad }}.{{ ext_upper }}
```

Output:
```text
~/Pictures/img_today/DSC0986.JPG -> ~/Pictures/output/555555/555555_01.JPG
~/Pictures/img_today/DSC0987.JPG -> ~/Pictures/output/555555/555555_02.JPG
~/Pictures/img_today/DSC0988.JPG -> ~/Pictures/output/555555/555555_03.JPG
```

### Template C (If Some Photos Come Before The QR)

If the QR code is first seen in `DSC0988.JPG`, then the earlier photos are unmatched:

```text
~/Pictures/img_today/DSC0986.JPG -> ~/Pictures/output/unmatched/img_today/DSC0986.JPG
~/Pictures/img_today/DSC0987.JPG -> ~/Pictures/output/unmatched/img_today/DSC0987.JPG
~/Pictures/img_today/DSC0988.JPG -> ~/Pictures/output/555555_101.JPG
```

### Template D (Include EXIF Timestamp)

Template:
```jinja2
{{ exif_date }}_{{ qrcode_safe }}_1{{ n_pad }}.{{ ext_upper }}
```

Run with `--exif`:
```sh
qrir --input ~/Pictures/img_today --output ~/Pictures/output --exif
```

Example output:
```text
~/Pictures/img_today/DSC0986.JPG -> ~/Pictures/output/2026-02-07_135501_555555_101.JPG
```

## Config

User config file (optional): `~/.qr_image_renamer.toml`

This repo includes a starter config: `.qr_image_renamer.toml`

Print the built-in defaults:

```sh
qrir --print-default-config
```

CLI flags override config values.

## Templating (MiniJinja)

The rename rule is a **MiniJinja** template. It must render a **relative path** (can include subfolders using `/`).

MiniJinja is by Armin Ronacher [mitsuhiko](https://github.com/mitsuhiko) and contributors. Thanks Armin.

Check out the respective docs for [syntax](https://docs.rs/minijinja/latest/minijinja/syntax/) and [filters](https://docs.rs/minijinja/latest/minijinja/filters/) if you want to use more advanced rules.

Safety rules (enforced):
- Template output must not be empty.
- No absolute paths.
- No `..` path components.

### MiniJinja Basics

Print a variable:
```jinja2
{{ qrcode_safe }}
```

Conditionals (useful for optional EXIF):
```jinja2
{% if exif_date %}{{ exif_date }}_{% endif %}{{ qrcode_safe }}_1{{ n_pad }}.{{ ext_upper }}
```

Filters:
```jinja2
{{ qrcode_safe | lower }}
{{ qrcode_raw | default("NOQR") }}
```

Note: `qrir` treats your config template as a **single inline template**. Template-loading features like `{% include %}` are not expected to work.

### Available Variables

| Name | Meaning |
| --- | --- |
| `qrcode_raw` | Raw decoded QR content. |
| `qrcode_safe` | Sanitized QR content safe for file/dir names. |
| `qrcode` | Alias for `qrcode_safe`. |
| `n0` | 0-based counter within current QR segment. |
| `n1` | 1-based counter within current QR segment. |
| `n` | Counter honoring `start_index` (0 or 1). |
| `n_pad` | `n` zero-padded to `padding` width. |
| `ext` | Extension (no dot), controlled by `ext_mode`/`ext_fixed`. |
| `ext_lower` | `ext` lowercased. |
| `ext_upper` | `ext` uppercased. |
| `orig_name` | Original filename (with extension). |
| `orig_stem` | Original filename stem (without extension). |
| `exif_date` | Formatted EXIF timestamp (empty if missing or `--exif` not set). |
| `src_rel_dir` | Directory path relative to `--input`. |
| `src_dir_name` | Leaf directory name containing the file. |
| `is_qr_frame` | True if this image contained a QR code. |
| `seq_index` | 0-based index within the directory after ordering. |
| `seq_total` | Total images in the directory after filtering and ordering. |

## Useful Flags

Progress:
```sh
qrir --input ~/Pictures/img_today --output ~/Pictures/output --progress
```

HTML gallery (writes `html_inspect.json` + `html_inspect.html` into `--output`):
```sh
qrir --input ~/Pictures/img_today --output ~/Pictures/output --html-inspect
```

Note: For the gallery to show images, do not use `--dry-run` (it needs the files in the output folder).

## Build (Bottom Section)

Build a release binary:

```sh
cargo build --release --bin qrir
./target/release/qrir --help
```

Dev:
```sh
cargo fmt
cargo clippy --all-targets --all-features
```

### Bench Summary (For Developers)

We ran local QR-decoder benchmarks on a real dataset and chose **ZXing (zxing-cpp)** as the only decoder:
- It was faster than the pure-Rust options we tested.
- It found at least as many QR codes as the alternatives on that dataset.
- Options tested were quircs, ZBar & rqrr
- YMMV so DYOR



### Licensing Notes (For Developers)

Not legal advice, but the practical checklist for shipping binaries:
1. Pick a license for *this* project and add it as `LICENSE`.
2. Collect third-party notices for all dependencies you distribute.
   - `minijinja` is **Apache-2.0**.
   - `zxing-cpp` (and the bundled ZXing C++ code) is **Apache-2.0**.
   - `nom-exif` is **MIT** (via `LICENSE` file in that crate).
   - Many common Rust crates are **MIT OR Apache-2.0** dual-licensed.
3. For Apache-2.0 components: include the Apache 2.0 license text and preserve required notices.
4. For MIT components: include the MIT license text and copyright notice.

If you want to automate this later, look into generating a `THIRD_PARTY_NOTICES.md` from `Cargo.lock` (e.g. using `cargo-about` or similar).

See also: `THIRD_PARTY_NOTICES.md`.

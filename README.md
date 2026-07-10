<p align="center">
  <a href="https://veilpdf.com">
    <img src="icon.png" alt="VeilPDF" width="128" height="128">
  </a>
</p>

<h1 align="center">veilpdf-core</h1>

<p align="center">
  <strong>An auditable, offline PDF engine written in Rust.</strong>
</p>

<p align="center">
  Merge, split, compress, sanitize, extract, watermark, and redact PDFs<br>
  without accounts, cloud services, or network dependencies.
</p>

<p align="center">
  <a href="https://github.com/iMADDDDDD/veilpdf-core/actions/workflows/ci.yml"><img src="https://github.com/iMADDDDDD/veilpdf-core/actions/workflows/ci.yml/badge.svg" alt="CI status"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT-green.svg" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/Rust-stable-black.svg" alt="Rust: stable">
  <img src="https://img.shields.io/badge/network-none-brightgreen.svg" alt="Network dependencies: none">
</p>

<p align="center">
  <a href="https://veilpdf.com">Website</a> &middot;
  <a href="#quick-start">Quick start</a> &middot;
  <a href="#operations">Operations</a> &middot;
  <a href="#security-model">Security model</a> &middot;
  <a href="#c-ffi">C FFI</a>
</p>

---

`veilpdf-core` is the document-processing engine behind
[VeilPDF](https://veilpdf.com), a native macOS app with 47 PDF tools. The core
is published separately under the MIT license so its handling of raw document
bytes can be inspected, tested, and embedded independently of the app.

## Why this exists

PDFs often contain contracts, financial records, identity documents, and other
sensitive data. A PDF engine should not need an account, an HTTP client, or a
cloud SDK to transform those files.

This crate keeps that boundary deliberately small:

- **No networking:** no HTTP client, TLS stack, socket use, analytics, or cloud SDK.
- **Native Rust API:** use the engine directly from Rust.
- **C ABI:** build a static library for Swift, Objective-C, C, or other FFI hosts.
- **Defensive parsing:** reject encrypted documents and bound expensive object, stream, image, and input sizes.
- **Auditable scope:** the repository contains the byte-level engine, tests, fixtures, and CI needed to review it.

The no-network guarantee applies to this crate. An application embedding the
library may still perform its own networking outside `veilpdf-core`.

## Quick start

The crate is currently distributed from GitHub:

```toml
[dependencies]
veilpdf-core = { git = "https://github.com/iMADDDDDD/veilpdf-core", branch = "main" }
```

Pin a commit with `rev` when reproducible builds are required.

```rust
use veilpdf_core::{compress_pdf, merge_pdfs, split_pdf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let merged = merge_pdfs(&["chapter-1.pdf", "chapter-2.pdf"])?;
    std::fs::write("book.pdf", merged)?;

    let pages = split_pdf("book.pdf")?;
    for (index, page) in pages.iter().enumerate() {
        std::fs::write(format!("page-{}.pdf", index + 1), page)?;
    }

    let compressed = compress_pdf("book.pdf")?;
    std::fs::write("book-small.pdf", compressed.data)?;

    println!("Reduced by {:.1}%", compressed.reduction_percent);
    Ok(())
}
```

The public API also provides byte-slice variants for hosts that do not want the
core to open files directly.

## Operations

| Operation | Rust API | Behavior |
| --- | --- | --- |
| Merge | `merge_pdfs`, `merge_pdfs_from_bytes` | Combines documents while preserving inherited page attributes |
| Split | `split_pdf`, `split_pdf_from_bytes` | Exports each page as an independent PDF |
| Compress | `compress_pdf`, `compress_pdf_with_options` | Recompresses and downsamples supported image streams |
| Sanitize | `sanitize_pdf` | Removes JavaScript, actions, embedded files, and optional metadata |
| Extract images | `extract_images` | Passes JPEGs through and converts supported Flate streams to PNG |
| Watermark | `apply_text_watermark` | Adds shaped Unicode text using an embedded subsetted TrueType font |
| Redact | `apply_redactions` | Removes covered text bytes and paints page-space redaction rectangles |
| Remove annotations | `remove_annotations` | Filters selected annotation classes while preserving form widgets |

### Compression options

```rust
use veilpdf_core::{compress_pdf_with_options, CompressOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::fs::read("input.pdf")?;
    let result = compress_pdf_with_options(
        &input,
        &CompressOptions {
            image_quality: 60,
            max_image_dimension: 1600,
            target_dpi: 120,
            strip_metadata: true,
        },
    )?;

    std::fs::write("output.pdf", result.data)?;
    Ok(())
}
```

### Sanitizing untrusted PDFs

```rust
use veilpdf_core::sanitize::{
    sanitize_pdf, FLAG_REMOVE_ACTIONS, FLAG_REMOVE_JS, FLAG_STRIP_METADATA,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::fs::read("untrusted.pdf")?;
    let output = sanitize_pdf(
        &input,
        FLAG_REMOVE_JS | FLAG_REMOVE_ACTIONS | FLAG_STRIP_METADATA,
    )?;
    std::fs::write("sanitized.pdf", output)?;
    Ok(())
}
```

| Flag | Additional sweep |
| --- | --- |
| `FLAG_STRIP_METADATA` | Removes the `/Info` dictionary |
| `FLAG_REMOVE_JS` | Removes JavaScript entries and named JavaScript destinations |
| `FLAG_REMOVE_EMBEDDED` | Removes embedded-file trees and file-attachment annotations |
| `FLAG_REMOVE_ACTIONS` | Removes document, page, and annotation action chains |
| `FLAG_REMOVE_XMP` | Removes XMP metadata streams |

JavaScript, actions, and embedded files are always removed by `sanitize_pdf`.
Flags add further sweeps; they do not disable the safety baseline.

## Security model

`veilpdf-core` treats every input document as untrusted.

| Defense | Limit or behavior |
| --- | --- |
| Encrypted documents | Rejected with a typed error |
| Input at the FFI boundary | 512 MB maximum per document |
| PDF object count | 500,000 objects maximum |
| Deflate streams | 256 MB decompressed maximum |
| Decoded images | 100 megapixels maximum |
| FFI panics | Contained with `catch_unwind` |
| Sanitization baseline | JavaScript, action chains, and embedded files always removed |

These controls reduce resource-exhaustion and parser-abuse risk; they are not a
claim that arbitrary PDFs are harmless. Please include a minimal reproducer
when reporting a malformed-document bug. Do not attach sensitive documents to
public issues.

## Architecture

```text
Rust or host application
          |
          | Rust API or C ABI
          v
+--------------------------------------------------+
|                  veilpdf-core                    |
|--------------------------------------------------|
| merge | split | compress | sanitize | extract   |
| watermark | redact | annotation filtering       |
|--------------------------------------------------|
| input limits | encrypted-PDF rejection          |
| bounded decompression | panic containment        |
+--------------------------------------------------+
          |
          v
     PDF output bytes
```

The focused dependency set is visible in [`Cargo.toml`](Cargo.toml):

| Crate | Purpose |
| --- | --- |
| [`lopdf`](https://crates.io/crates/lopdf) | PDF objects, page trees, and serialization |
| [`image`](https://crates.io/crates/image) | JPEG and PNG decoding/encoding |
| [`flate2`](https://crates.io/crates/flate2) | Bounded deflate decompression |
| [`ttf-parser`](https://crates.io/crates/ttf-parser) | TrueType metrics |
| [`subsetter`](https://crates.io/crates/subsetter) | Embedded font subsetting |
| [`rustybuzz`](https://crates.io/crates/rustybuzz) | Unicode text shaping |
| [`unicode-bidi`](https://crates.io/crates/unicode-bidi) | Bidirectional text ordering |

There is no async runtime, TLS client, serialization framework, or networking
crate in the dependency graph.

## C FFI

The crate builds both an `rlib` and a static library:

```bash
cargo build --release
# target/release/libveilpdf_core.a
```

The C ABI exposes merge, split, compression, sanitization, annotation removal,
image extraction, watermarking, and redaction entry points. See
[`src/ffi.rs`](src/ffi.rs) for the `#[repr(C)]` types and authoritative function
signatures.

Every exported operation catches Rust panics before returning to foreign code.
Buffers returned through the ABI must be released with `veil_free_buffer`.

## Development

Install the current stable Rust toolchain, clone the repository, and run:

```bash
cargo build --release
cargo test --release
cargo clippy --all-targets --release -- -D warnings
cargo fmt --all -- --check
```

The release profile intentionally uses `panic = "unwind"`. The FFI layer relies
on unwinding to contain panics with `catch_unwind`; changing it to `abort` would
remove that boundary.

## Relationship to the VeilPDF app

[VeilPDF](https://veilpdf.com) is a native macOS application with 47 PDF tools.
It offers a free three-day trial, followed by a $29 one-time purchase with no
subscription.

The app adds a proprietary SwiftUI interface and workflows implemented with
Apple frameworks such as PDFKit, Core Image, WebKit, and NSAttributedString.
This repository contains the MIT-licensed Rust engine responsible for its
low-level PDF byte processing; it is not the complete application source.

## Contributing

Bug fixes, security hardening, compatibility improvements, and focused test
coverage are welcome. For substantial changes, open an issue first so the scope
and API impact can be discussed.

Before opening a pull request, run the commands in [Development](#development).
CI checks release builds and tests on Linux and macOS, plus strict Clippy on
stable Rust.

Use [GitHub Issues](https://github.com/iMADDDDDD/veilpdf-core/issues) for bugs
and feature proposals. Never upload confidential PDFs; reduce a failing file to
a nonsensitive fixture whenever possible.

## License

`veilpdf-core` is available under the [MIT License](LICENSE-MIT).

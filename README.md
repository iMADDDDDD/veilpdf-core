<p align="center">
  <img src="icon.png" alt="VeilPDF" width="128" height="128">
</p>

<h1 align="center">veilpdf-core</h1>

<p align="center">
  <strong>Privacy-first PDF engine. Your documents never leave your machine.</strong>
</p>

<p align="center">
  Merge &middot; Split &middot; Compress &middot; Sanitize &middot; Extract<br>
  <sub>Pure Rust &middot; Under 2,000 lines &middot; MIT licensed</sub>
</p>

<p align="center">
  <a href="https://github.com/iMADDDDDD/veilpdf-core/actions/workflows/ci.yml"><img src="https://github.com/iMADDDDDD/veilpdf-core/actions/workflows/ci.yml/badge.svg" alt="CI status"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT-green.svg" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/network-zero-brightgreen.svg" alt="Network: zero">
  <img src="https://img.shields.io/badge/unsafe-FFI%20boundary%20only-blue.svg" alt="Unsafe: FFI boundary only">
</p>

---

## At a glance

- **Zero networking.** No `reqwest`, no `hyper`, no sockets. Cannot phone home by construction.
- **Three dependencies.** `lopdf`, `image`, `flate2`. No async runtime, no TLS, no serialization framework.
- **Under 2,000 lines.** An afternoon to read the entire codebase end to end.
- **Hostile-input safe.** Encrypted PDFs rejected, zip bombs capped, megapixel-bombed images rejected, FFI panics contained.
- **Rust + C ABI.** Use as a Rust crate or link the static `.a` from Swift, Objective-C, or C.

## Why

Most PDF libraries pull in HTTP clients, async runtimes, or cloud SDKs. If you're processing sensitive documents — tax returns, contracts, medical records — that's a liability.

`veilpdf-core` is the engine behind [VeilPDF](https://veilpdf.com) on the Mac App Store. The app is closed source; this crate is the byte-level engine, open-sourced so anyone can audit what actually happens to your files.

## Operations

| Operation | What it does | Notes |
|-----------|-------------|-------|
| **Merge** | Combine multiple PDFs into one | Preserves inherited page attributes (MediaBox, Resources) |
| **Split** | Extract each page as a standalone PDF | Same inheritance handling as merge |
| **Compress** | Reduce file size | JPEG recompression with ColorSpace classification; SMask/Mask images preserved |
| **Sanitize** | Strip dangerous content | Always removes JS, actions, embedded files; optional XMP and `/Info` sweep |
| **Extract** | Pull images out of a PDF | JPEG passthrough; FlateDecode → PNG |

## Hostile-input defenses

- **Encrypted PDF rejection** — password-protected files return a clear error instead of silently producing garbage.
- **Object-count cap** — 500,000-object ceiling blocks object-stream amplification attacks that would otherwise hang every downstream operation.
- **Bounded decompression** — 256 MB limit on deflate streams, driven by `flate2` (lopdf's built-in `decompress` silently fails on real-world files).
- **Image decode cap** — 100 megapixel limit on decoded images stops OOM-style malicious streams.
- **FFI panic safety** — every C entry point wrapped in `catch_unwind`; panics cannot unwind into foreign code.
- **Input size cap** — 512 MB per document at the FFI boundary.
- **Sanitize safety baseline** — JS, action chains, and embedded files are *always* stripped by `sanitize_pdf`, regardless of flags. Callers can add sweeps but cannot opt out.

## Quick start (Rust)

```toml
[dependencies]
veilpdf-core = { git = "https://github.com/iMADDDDDD/veilpdf-core" }
```

```rust
use veilpdf_core::{merge_pdfs, split_pdf, compress_pdf};

// Merge
let merged = merge_pdfs(&["a.pdf", "b.pdf"]).unwrap();
std::fs::write("merged.pdf", merged).unwrap();

// Split into individual pages
let pages = split_pdf("document.pdf").unwrap();
for (i, page) in pages.iter().enumerate() {
    std::fs::write(format!("page_{}.pdf", i + 1), page).unwrap();
}

// Compress
let result = compress_pdf("large.pdf").unwrap();
std::fs::write("small.pdf", result.data).unwrap();
println!("Reduced by {:.1}%", result.reduction_percent);
```

<details>
<summary><strong>Advanced compression options</strong></summary>

```rust
use veilpdf_core::{compress_pdf_with_options, CompressOptions};

let data = std::fs::read("input.pdf").unwrap();
let result = compress_pdf_with_options(&data, &CompressOptions {
    image_quality: 60,          // JPEG quality (1-100)
    max_image_dimension: 1600,  // Downscale images larger than this
    strip_metadata: true,       // Remove author, creation date, XMP, thumbnails
}).unwrap();

println!("{:.1} MB -> {:.1} MB ({:.1}% reduction)",
    result.input_size as f64 / 1_048_576.0,
    result.output_size as f64 / 1_048_576.0,
    result.reduction_percent);
```

</details>

<details>
<summary><strong>Sanitize untrusted PDFs</strong></summary>

```rust
use veilpdf_core::sanitize::{sanitize_pdf, FLAG_REMOVE_JS, FLAG_REMOVE_ACTIONS, FLAG_STRIP_METADATA};

let data = std::fs::read("untrusted.pdf").unwrap();
let clean = sanitize_pdf(&data, FLAG_REMOVE_JS | FLAG_REMOVE_ACTIONS | FLAG_STRIP_METADATA).unwrap();
std::fs::write("clean.pdf", clean).unwrap();
```

| Flag | Removes |
|------|---------|
| `FLAG_STRIP_METADATA` | `/Info` dict (author, title, creation date, producer) |
| `FLAG_REMOVE_JS` | `/JS`, `/JavaScript`, and named-JavaScript destinations |
| `FLAG_REMOVE_EMBEDDED` | `/EmbeddedFiles` and `/FileAttachment` annotations |
| `FLAG_REMOVE_ACTIONS` | `/OpenAction`, `/AA`, and link-annotation actions |
| `FLAG_REMOVE_XMP` | `/Metadata` streams (both `/Type /Metadata` and `/Subtype /XML`) |

JS, actions, and embedded files are stripped unconditionally — the flags argument only *adds* sweeps, never opts out of the safety baseline.

</details>

<details>
<summary><strong>Extract images</strong></summary>

```rust
use veilpdf_core::extract_images;

let data = std::fs::read("document.pdf").unwrap();
let images = extract_images(&data).unwrap();
for (i, img) in images.iter().enumerate() {
    let ext = if img.format == 0 { "jpg" } else { "png" };
    std::fs::write(format!("image_{}.{}", i + 1, ext), &img.data).unwrap();
    println!("{}x{} {}", img.width, img.height, ext.to_uppercase());
}
```

JPEG (`DCTDecode`) images are passed through unchanged. Flate-compressed images are decoded and re-encoded as PNG.

</details>

## C FFI

The crate builds as a static library (`libveilpdf_core.a`) and exposes a C ABI for embedding in Swift, Objective-C, C, or any language with C interop.

```c
VeilBuffer veil_merge(const uint8_t *a, size_t a_len, const uint8_t *b, size_t b_len);
VeilBuffer veil_split(const uint8_t *ptr, size_t len);
VeilBuffer veil_compress(const uint8_t *ptr, size_t len);
VeilBuffer veil_compress_ex(const uint8_t *ptr, size_t len,
                            uint8_t quality, uint32_t max_dim, uint8_t strip_meta);
VeilBuffer veil_sanitize(const uint8_t *ptr, size_t len, uint32_t flags);
VeilBuffer veil_extract_images(const uint8_t *ptr, size_t len);
void veil_free_buffer(VeilBuffer buf);
```

```bash
cargo build --release
# target/release/libveilpdf_core.a
```

Every FFI entry point is wrapped in `catch_unwind`. Panics cannot cross the boundary.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    veilpdf-core  (MIT)                    │
├──────────────────────────────────────────────────────────┤
│   merge      split      compress     sanitize    extract  │
│   lopdf      lopdf      lopdf        lopdf       lopdf    │
│                         image                    image    │
│                         flate2                            │
├──────────────────────────────────────────────────────────┤
│   limits  ·  encrypted-PDF rejection  ·  object-count cap │
├──────────────────────────────────────────────────────────┤
│     ffi   ·   C ABI   ·   catch_unwind   ·   unwind       │
└──────────────────────────────────────────────────────────┘
```

### Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| [lopdf](https://crates.io/crates/lopdf) | 0.34 | Pure-Rust PDF object manipulation — page trees, object remapping |
| [image](https://crates.io/crates/image) | 0.25 | JPEG + PNG codecs (minimal feature flags) for recompression |
| [flate2](https://crates.io/crates/flate2) | 1 | Bounded deflate decompression that rejects zip bombs |

No async runtime. No TLS. No serialization framework. No network of any kind.

## Development

```bash
cargo build --release          # static lib + rlib
cargo test --release           # 21 integration + unit tests
cargo clippy -- -D warnings    # lint strict
cargo doc --open               # browse the public API
```

> **On `panic = "unwind"`**
>
> The release profile in `Cargo.toml` pins `panic = "unwind"` because `ffi.rs` uses `catch_unwind` to contain panics at the C boundary. Switching to `panic = "abort"` silently removes that safety guarantee — don't.

## The macOS app

This crate is the byte-level engine of [**VeilPDF**](https://veilpdf.com) — a native macOS app with 47 PDF tools, one-time $29 on the Mac App Store. The app adds 42 higher-level tools on top of this engine using Apple's own frameworks:

- **PDFKit** — page organization, rotation, annotations, form filling, signatures, stamps, watermarks, page numbers, passwords, permissions, bookmarks, metadata, flattening, diffing, repair
- **Core Image** — contrast, scanner effect, color replacement
- **WebKit** — HTML to PDF
- **NSAttributedString** — Markdown to PDF

The app source is proprietary; this crate — the part that touches your raw document bytes — is the auditable open-source trust layer.

## Contributing

The surface is intentionally small. Bug fixes, safety hardening, and test coverage for existing operations are welcome. Before opening a PR:

```bash
cargo test --release && cargo clippy -- -D warnings
```

## License

MIT — see [LICENSE-MIT](LICENSE-MIT).

//! The NML formatter: one function, one style, no options.
//!
//! [`formatter::format_source`] takes source text and returns source text
//! in canonical style, with every comment in place. It is the whole public
//! surface of this crate; everything else here is how it is done.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use nml_fmt::formatter::format_source;
//!
//! let messy = "service  Api:\n  host   =  \"0.0.0.0\"  // keep me\n";
//! let once = format_source(messy)?;
//! assert_eq!(once, "service Api:\n    host = \"0.0.0.0\"  // keep me\n");
//! assert_eq!(format_source(&once)?, once, "formatting is idempotent");
//! # Ok(())
//! # }
//! ```
//!
//! # What it owns, and what it leaves alone
//!
//! LAYOUT is regenerated: indentation, the line breaks the grammar
//! requires, the spacing between tokens on a line, trailing whitespace.
//! STRUCTURE and MEANING are never touched: nothing is reordered, no
//! entry is added or removed, no type expression is restructured, no
//! value changes.
//!
//! Between the two lies what the grammar leaves free and a reader can
//! read something into — a blank line, an aligned `->` column, a string's
//! delimiter, whether a value sits on its `=`'s line or the next one.
//! **Those belong to the author and are preserved**, which also means the
//! formatter never invents them: it aligns nothing on its own, and so it
//! never repairs an alignment an edit broke.
//!
//! An invalid document is REFUSED rather than rewritten from a recovered
//! tree, as `gofmt` and `rustfmt` refuse: the caller leaves the file
//! untouched and reports the error.
//!
//! The style is specified, normatively, in `spec/style.md` (which also
//! lists the eight guarantees this crate's property battery holds it to).
//! Embedding it in your own `fmt` subcommand: see the cookbook's
//! *Format user files idempotently*.

// No `unsafe` in this crate (RFC 0019 item 0, E35): enforced at the root.
#![forbid(unsafe_code)]

pub mod formatter;
mod printer;

//! The serializer in every encoding: as `serialize`, but with the encoding
//! chosen by the script from those the file's BOM allows, so each
//! single-byte encoding (including those detection never picks) and UTF-16
//! (read-only; Save As UTF-8 converts it) goes through detection, editing
//! with characters the encoding has and lacks, saving and reopening. See
//! `src/harness.rs`.

#![no_main]

use leal_fuzz::harness::{Target, run};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| run(data, Target::Encodings));

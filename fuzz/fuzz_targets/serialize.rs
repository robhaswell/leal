//! The serializer: a file and an edit script (cell edits, row and column
//! inserts and deletes, undo and redo), on a real document and on the save
//! oracle, saved to memory: the oracle's bytes, splices and fixes, or its
//! refusal; the saved file reopens with the oracle's values; and undoing
//! everything saves the original bytes (F1–F6). The encoding is
//! detection's. See `src/harness.rs`.

#![no_main]

use leal_fuzz::harness::{Target, run};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| run(data, Target::Serialize));

//! The whole-file part of ADR-0008 decision 8 (task 2.5.3b): a save
//! records the delimiter and header choice in Leal's interpretation
//! attribute when a reopen would guess them differently. A save can tell
//! for first paint at once (`save::AttributePlan`), but a reopen's
//! whole-file review needs the review of the new file, about half a second
//! per 100 MB, which doesn't fit a save's budget. The reading a save makes
//! runs that very review anyway, so when it suggests another delimiter,
//! the attribute is written then, onto the saved file.

use std::io;
use std::sync::atomic::Ordering;

use crate::attributes::{Fingerprint, Interpretation};
use crate::source::{INTERPRETATION_ATTRIBUTE_C, OriginalState, look_afresh, same_file};

use super::Document;

impl Document {
    /// Records the delimiter and header choice, with the saved bytes'
    /// fingerprint (ADR-0007), in the interpretation attribute of the file
    /// the last save wrote, when the review of that save's reading
    /// suggests another delimiter: a reopen's review would, too.
    ///
    /// It writes only when:
    /// - no save is under way;
    /// - the current reading is the one a save made (not read again since);
    /// - its review has finished and suggests another delimiter;
    /// - the save didn't record the delimiter already, nor this did;
    /// - the file is still the one that reading read, where it is now (it
    ///   may have been moved): the same volume, inode, length and
    ///   modification time as the reading's own source, and the watcher
    ///   has seen no change. Not the watcher's identity: a later save whose
    ///   file couldn't be read back moves the watcher on to its file but
    ///   leaves this reading current, and its attribute must stay.
    ///
    /// A recorded encoding (ADR-0013 decision 2) is kept. Once written, the
    /// delimiter and header count as the attribute's choice, so every later
    /// save records them itself (`Reading::remembered`). Returns whether
    /// it wrote the attribute. It looks at the file, which a network share
    /// can slow down: call it off the main thread.
    ///
    /// # Errors
    ///
    /// If the file can't be looked at, or the system refuses the attribute.
    pub fn remember_reviewed_interpretation(&self) -> io::Result<bool> {
        // A save under way replaces the file, and makes a reading of its
        // own, which is reviewed in turn.
        if self.saving.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let reading = self.current();
        if !reading.saved || reading.remembered.load(Ordering::Acquire) {
            return Ok(false);
        }
        let Some(Ok(review)) = reading.review_job.result() else {
            return Ok(false);
        };
        if review.delimiter_suggestion.is_none() {
            return Ok(false);
        }
        let recorded = reading
            .source
            .attributes()
            .interpretation
            .as_deref()
            .and_then(|value| Interpretation::parse(value).ok());
        if recorded.is_some_and(|recorded| recorded.delimiter.is_some()) {
            return Ok(false);
        }
        let status = self.original.status();
        if status.diverged || status.state != OriginalState::Unchanged {
            return Ok(false);
        }
        let value = Interpretation {
            delimiter: Some(reading.detection.delimiter),
            header: Some(reading.detection.header),
            file: Some(Fingerprint::from_head(&reading.head, reading.source.len())),
            encoding: recorded.and_then(|recorded| recorded.encoding),
        }
        .to_attribute_value();
        let Some(existing) = look_afresh(&status.path)? else {
            return Ok(false);
        };
        let read = reading.source.identity();
        let now = existing.identity();
        // `same_file` leaves out the volume, for a removable drive mounted
        // again; here the file was saved on this mount.
        if read.device != now.device || !same_file(read, now) {
            return Ok(false);
        }
        existing.set_attribute(INTERPRETATION_ATTRIBUTE_C, value.as_bytes())?;
        reading.remembered.store(true, Ordering::Release);
        Ok(true)
    }
}

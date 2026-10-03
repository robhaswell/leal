//! Text into bytes for a save (task 2.3, DESIGN §3.7, F5): an edited
//! value in the file's encoding ([`encode`]), and, for Save As UTF-8
//! (ADR-0008 decision 7), the file's own bytes in UTF-8 ([`Transcoder`]).
//!
//! **Single-byte encodings** (ADR-0005 decision 5). Each is a table of what
//! every byte means, built once from the decoder that shows the file's
//! values (`encoding_rs`'s WHATWG tables; ISO-8859-1 by hand, where every
//! byte is the code point of the same value). Encoding inverts that table,
//! so a character is written as the byte that reads back as it, and only
//! then: a value encodes to bytes that show it again, exactly, and a value
//! shown from the file encodes to the bytes it came from (a byte the
//! encoding leaves unassigned shows as U+FFFD, which no single-byte
//! encoding can write). Nothing is ever substituted: a character the
//! encoding has no byte for is [`Unencodable`].
//!
//! **UTF-16** files are read-only in v1 (DESIGN §1, §4.3): no save writes
//! UTF-16. Save As UTF-8 converts them.

use std::borrow::Cow;
use std::fmt;
use std::sync::OnceLock;

use crate::dialect::Encoding;

/// A character `encoding` can't represent, so a save in it would lose or
/// substitute it (F5): the save stops instead, and names the cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unencodable {
    /// The encoding.
    pub encoding: Encoding,
    /// The first character of the value it has no bytes for.
    pub character: char,
}

impl fmt::Display for Unencodable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} (U+{:04X}) can't be saved in {}",
            self.character,
            u32::from(self.character),
            self.encoding.iana_name()
        )
    }
}

impl std::error::Error for Unencodable {}

/// `value`'s bytes in `encoding`, or the first character it can't
/// represent. Borrowed when the bytes are the value's own (UTF-8, and ASCII
/// in a single-byte encoding). UTF-16 can represent anything; the save
/// never writes it (v1), but the answer is still the honest one.
///
/// # Errors
///
/// [`Unencodable`], with the first character `encoding` has no byte for.
///
/// ```
/// use leal_core::dialect::Encoding;
/// use leal_core::save::encode;
/// assert_eq!(encode("café", Encoding::Windows1252).unwrap(), &b"caf\xE9"[..]);
/// assert_eq!(encode("€", Encoding::MacRoman).unwrap(), &b"\xDB"[..]);
/// let refused = encode("a😀", Encoding::Windows1252).unwrap_err();
/// assert_eq!(refused.character, '😀');
/// ```
pub fn encode(value: &str, encoding: Encoding) -> Result<Cow<'_, [u8]>, Unencodable> {
    match encoding {
        Encoding::Utf8 => Ok(Cow::Borrowed(value.as_bytes())),
        Encoding::Utf16Le => Ok(Cow::Owned(
            value.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        )),
        Encoding::Utf16Be => Ok(Cow::Owned(
            value.encode_utf16().flat_map(u16::to_be_bytes).collect(),
        )),
        single_byte => {
            if value.is_ascii() {
                return Ok(Cow::Borrowed(value.as_bytes()));
            }
            let table = SingleByte::of(single_byte);
            value
                .chars()
                .map(|character| {
                    table.byte(character).ok_or(Unencodable {
                        encoding,
                        character,
                    })
                })
                .collect::<Result<Vec<u8>, _>>()
                .map(Cow::Owned)
        }
    }
}

/// What each byte of a single-byte encoding means.
#[derive(Debug)]
struct SingleByte {
    /// Each byte's character in UTF-8 (one to three bytes), or `None` if
    /// the encoding leaves it unassigned.
    utf8: [Option<([u8; 3], u8)>; 256],
    /// The characters other than ASCII, sorted, each with its byte.
    bytes: Vec<(char, u8)>,
}

impl SingleByte {
    /// The table for `encoding`, a single-byte encoding, built at first use.
    fn of(encoding: Encoding) -> &'static SingleByte {
        static TABLES: OnceLock<Vec<(Encoding, SingleByte)>> = OnceLock::new();
        let tables = TABLES.get_or_init(|| {
            Encoding::ALL
                .into_iter()
                .filter(|e| e.is_ascii_compatible() && *e != Encoding::Utf8)
                .map(|e| (e, SingleByte::build(e)))
                .collect()
        });
        tables
            .iter()
            .find(|(e, _)| *e == encoding)
            .or_else(|| tables.first())
            .map(|(_, table)| table)
            .unwrap_or_else(|| unreachable!("there are single-byte encodings"))
    }

    fn build(encoding: Encoding) -> SingleByte {
        let mut utf8 = [None; 256];
        let mut bytes = Vec::new();
        for (byte, slot) in (0..=u8::MAX).zip(utf8.iter_mut()) {
            let character = match encoding.whatwg() {
                Some(decoder) => decoder
                    .decode_without_bom_handling_and_without_replacement(&[byte])
                    .and_then(|text| text.chars().next()),
                // ISO-8859-1, which the WHATWG standard reads as
                // Windows-1252: every byte is the code point of the same
                // value, as display values decode it.
                None => Some(char::from(byte)),
            };
            let Some(character) = character else {
                continue;
            };
            let mut buffer = [0; 4];
            let len = character.encode_utf8(&mut buffer).len();
            let mut three = [0; 3];
            three[..len.min(3)].copy_from_slice(&buffer[..len.min(3)]);
            *slot = Some((three, u8::try_from(len).unwrap_or(3)));
            if !character.is_ascii() {
                bytes.push((character, byte));
            }
        }
        bytes.sort_unstable();
        SingleByte { utf8, bytes }
    }

    /// The byte that reads as `character`, if any.
    fn byte(&self, character: char) -> Option<u8> {
        if character.is_ascii() {
            return u8::try_from(u32::from(character)).ok();
        }
        self.bytes
            .binary_search_by_key(&character, |&(c, _)| c)
            .ok()
            .map(|at| self.bytes[at].1)
    }
}

/// Converts the file's bytes to UTF-8, for Save As UTF-8 (ADR-0008
/// decision 7), a stretch at a time, never replacing anything: each
/// sequence that isn't text in the file's encoding (an unpaired surrogate
/// or a final odd byte in UTF-16, a byte a single-byte encoding leaves
/// unassigned) is reported by its offset in the file instead, and nothing
/// is written for it.
pub(crate) struct Transcoder {
    source: Source,
}

enum Source {
    SingleByte(&'static SingleByte),
    Utf16 {
        encoding: &'static encoding_rs::Encoding,
        decoder: encoding_rs::Decoder,
    },
}

impl Transcoder {
    /// A transcoder from `encoding`, or `None` for UTF-8, whose bytes are
    /// kept as they are (invalid ones too, F4).
    pub(crate) fn new(encoding: Encoding) -> Option<Transcoder> {
        let source = match encoding {
            Encoding::Utf8 => return None,
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let encoding = encoding.whatwg()?;
                Source::Utf16 {
                    encoding,
                    decoder: encoding.new_decoder_without_bom_handling(),
                }
            }
            single_byte => Source::SingleByte(SingleByte::of(single_byte)),
        };
        Some(Transcoder { source })
    }

    /// Converts `bytes`, the file's from offset `at` on, adding the UTF-8
    /// to `out` and calling `bad` with the offset of each sequence that
    /// isn't text. `end` says the stretch ends here on a whole character
    /// (or at the end of the file): UTF-16 then has nothing pending, and a
    /// surrogate or byte left over is reported. Between the pieces of one
    /// stretch, a surrogate pair or a code unit may be split.
    pub(crate) fn push(
        &mut self,
        bytes: &[u8],
        at: usize,
        end: bool,
        out: &mut Vec<u8>,
        bad: &mut dyn FnMut(usize),
    ) {
        match &mut self.source {
            Source::SingleByte(table) => {
                let mut i = 0;
                while i < bytes.len() {
                    // ASCII runs are copied as they are.
                    let run = bytes[i..]
                        .iter()
                        .position(|b| !b.is_ascii())
                        .unwrap_or(bytes.len() - i);
                    out.extend_from_slice(&bytes[i..i + run]);
                    i += run;
                    if i < bytes.len() {
                        match table.utf8[usize::from(bytes[i])] {
                            Some((utf8, len)) => {
                                out.extend_from_slice(&utf8[..usize::from(len)]);
                            }
                            None => bad(at + i),
                        }
                        i += 1;
                    }
                }
            }
            Source::Utf16 { encoding, decoder } => {
                let mut read_so_far = 0;
                loop {
                    let rest = &bytes[read_so_far..];
                    let room = decoder
                        .max_utf8_buffer_length_without_replacement(rest.len())
                        .unwrap_or(rest.len().saturating_mul(3).saturating_add(4));
                    let start = out.len();
                    out.resize(start + room, 0);
                    let (result, read, written) =
                        decoder.decode_to_utf8_without_replacement(rest, &mut out[start..], end);
                    out.truncate(start + written);
                    read_so_far += read;
                    match result {
                        encoding_rs::DecoderResult::InputEmpty => break,
                        // Can't happen with the room the decoder asked
                        // for; if it did with no progress, it would loop.
                        encoding_rs::DecoderResult::OutputFull if read == 0 && written == 0 => {
                            bad(at + read_so_far);
                            break;
                        }
                        encoding_rs::DecoderResult::OutputFull => {}
                        encoding_rs::DecoderResult::Malformed(len, after) => {
                            // The bad sequence ends `after` bytes before
                            // where reading stopped; it may have begun in
                            // an earlier piece.
                            let stop = at + read_so_far;
                            bad(stop.saturating_sub(usize::from(after) + usize::from(len)));
                        }
                    }
                }
                if end {
                    *decoder = encoding.new_decoder_without_bom_handling();
                }
            }
        }
    }

    /// All of `bytes` (a whole field) in UTF-8, or `None` if any of it
    /// isn't text in `encoding`. Borrowed from UTF-8.
    pub(crate) fn convert(bytes: &[u8], encoding: Encoding) -> Option<Cow<'_, [u8]>> {
        let Some(mut transcoder) = Transcoder::new(encoding) else {
            return Some(Cow::Borrowed(bytes));
        };
        let mut out = Vec::with_capacity(bytes.len());
        let mut fine = true;
        transcoder.push(bytes, 0, true, &mut out, &mut |_| fine = false);
        fine.then_some(Cow::Owned(out))
    }
}

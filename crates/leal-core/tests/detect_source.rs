//! End to end: a real file with real extended attributes, opened by
//! `Source::open`, then detected from its bytes and raw attributes.

use std::path::PathBuf;
use std::process::Command;

use leal_core::attributes::{Fingerprint, Interpretation};
use leal_core::detect::{
    Choices, DialectSource, EncodingSource, FIRST_PAINT_BYTES, Hints, Note, detect,
};
use leal_core::dialect::{Delimiter, Encoding};
use leal_core::source::{INTERPRETATION_ATTRIBUTE, Source, TEXT_ENCODING_ATTRIBUTE, TempFolders};

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("leal-detect-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }

    fn temp(&self) -> TempFolders {
        TempFolders::new(self.0.join("scratch"), self.0.join("records"))
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn set_attribute(path: &std::path::Path, name: &str, value: &[u8]) {
    let hex: String = value.iter().map(|b| format!("{b:02x}")).collect();
    let status = Command::new("/usr/bin/xattr")
        .arg("-wx")
        .arg(name)
        .arg(hex)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn detection_reads_the_attributes_source_opens() {
    let dir = Dir::new("attributes");
    let path = dir.0.join("prices.csv");
    // Valid UTF-8 ("é"), guessed as semicolon-separated.
    let bytes = "a;b\ncafé;1\n".as_bytes();
    std::fs::write(&path, bytes).unwrap();
    set_attribute(&path, TEXT_ENCODING_ATTRIBUTE, b"windows-1252;1280");
    let remembered = Interpretation {
        delimiter: Some(Delimiter::Comma),
        header: Some(false),
        file: Some(Fingerprint::of(bytes)),
    };
    set_attribute(
        &path,
        INTERPRETATION_ATTRIBUTE,
        remembered.to_attribute_value().as_bytes(),
    );

    let source = Source::open(&path, &dir.temp(), None).unwrap();
    let hints = Hints::from(source.attributes());
    let head = source.read_range(0..FIRST_PAINT_BYTES).unwrap();
    let d = detect(&head, source.len(), hints, Choices::default()).unwrap();
    assert_eq!(
        (d.encoding, d.encoding_source),
        (Encoding::Windows1252, EncodingSource::Attribute)
    );
    assert_eq!(
        (d.delimiter, d.delimiter_source, d.header, d.header_source),
        (
            Delimiter::Comma,
            DialectSource::Attribute,
            false,
            DialectSource::Attribute
        )
    );
    assert_eq!(d.notes, []);
}

#[test]
fn detection_without_attributes_guesses() {
    let dir = Dir::new("plain");
    let path = dir.0.join("plain.csv");
    std::fs::write(&path, "a;b\n1;2\n").unwrap();
    set_attribute(&path, TEXT_ENCODING_ATTRIBUTE, b"utf-16;256");
    let source = Source::open(&path, &dir.temp(), None).unwrap();
    let d = detect(
        &source.read_range(0..FIRST_PAINT_BYTES).unwrap(),
        source.len(),
        source.attributes().into(),
        Choices::default(),
    )
    .unwrap();
    assert_eq!(
        (d.encoding_source, d.delimiter),
        (EncodingSource::Guess, Delimiter::Semicolon)
    );
    assert_eq!(d.notes, [Note::TextEncodingUtf16WithoutBom]);
}

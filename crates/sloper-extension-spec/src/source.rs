//! Pure source property, native filename, and media validation shared by the
//! store and extension host.

use std::fmt;

use serde::de::{
    MapAccess,
    SeqAccess,
    Visitor as DeVisitor,
};

/// Invalid direct source property identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("source property must be a declared identifier of one to 64 ASCII characters")]
pub struct SourcePropertyError;

impl SourcePropertyError {
    /// Reports an identifier that cannot name a direct source property.
    #[must_use]
    #[inline]
    pub const fn invalid() -> Self {
        Self
    }
}

/// Validates a direct source property name before accepting produced bytes.
///
/// # Errors
/// Rejects names outside `[A-Za-z_][A-Za-z0-9_]*` or one to 64 bytes.
///
/// ```
/// use sloper_extension_spec::validate_source_property;
/// assert!(validate_source_property("document").is_ok());
/// assert!(validate_source_property("/properties/document").is_err());
/// ```
pub fn validate_source_property(property: &str) -> Result<(), SourcePropertyError> {
    if property.is_empty()
        || property.len() > 64
        || !property.as_bytes()[0].is_ascii_alphabetic() && property.as_bytes()[0] != b'_'
        || !property
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(SourcePropertyError::invalid());
    }
    Ok(())
}

/// Invalid native source basename.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("source filename must be a native basename of at most 255 UTF-8 bytes")]
pub struct SourceFilenameError;

impl SourceFilenameError {
    /// Reports a basename outside the native source spec.
    #[must_use]
    pub const fn invalid() -> Self {
        Self
    }
}

/// Validates the immutable metadata basename before source bytes are accepted.
///
/// # Errors
/// Rejects an empty name, dot/dot-dot, separators, NUL or more than 255 bytes.
///
/// ```
/// use sloper_extension_spec::validate_source_filename;
/// assert!(validate_source_filename("invoice.pdf").is_ok());
/// assert!(validate_source_filename("folder/invoice.pdf").is_err());
/// ```
pub fn validate_source_filename(filename: &str) -> Result<(), SourceFilenameError> {
    if filename.is_empty()
        || filename.len() > 255
        || matches!(filename, "." | "..")
        || filename.bytes().any(|byte| matches!(byte, 0 | b'/' | b'\\'))
    {
        return Err(SourceFilenameError::invalid());
    }
    Ok(())
}

/// Matches a source media declaration against a detected concrete media type.
/// Type and subtype tokens are case-insensitive; a subtype wildcard matches
/// only its exact major type.
#[must_use]
pub fn source_media_matches(declared: &str, actual: &str) -> bool {
    declared.eq_ignore_ascii_case(actual)
        || declared.strip_suffix("/*").is_some_and(|major| {
            actual
                .split_once('/')
                .is_some_and(|(actual_major, subtype)| major.eq_ignore_ascii_case(actual_major) && !subtype.is_empty())
        })
}

/// Identifies native bytes without using the filename or rewriting content.
/// An incomplete or unrecognized archive remains application/zip.
///
/// ```
/// use sloper_extension_spec::detect_media_type;
/// assert_eq!(detect_media_type(b"%PDF-1.7\n"), Some("application/pdf"));
/// assert_eq!(detect_media_type(b""), None);
/// ```
#[must_use]
pub fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else if bytes.starts_with(b"\x1f\x8b") {
        Some("application/gzip")
    } else if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        Some(office_zip_media(bytes).unwrap_or("application/zip"))
    } else if !bytes.is_empty() && !bytes.contains(&0) {
        let text = match str::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() => str::from_utf8(&bytes[..error.valid_up_to()]).ok()?,
            Err(_) => return None,
        };
        let trimmed = text.trim_start();
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && serde_json::from_slice::<JsonProbe>(bytes).map_or_else(|error| error.is_eof(), |_| true)
        {
            Some("application/json")
        } else if (trimmed.starts_with("<?xml") || trimmed.starts_with("<svg")) && trimmed.contains("<svg") {
            Some("image/svg+xml")
        } else {
            let mut lines = text.lines().filter(|line| !line.is_empty());
            match (lines.next(), lines.next()) {
                (Some(first), Some(second))
                    if first.contains(',') && first.matches(',').count() == second.matches(',').count() =>
                {
                    Some("text/csv")
                },
                (Some(first), Some(second))
                    if first.contains('\t') && first.matches('\t').count() == second.matches('\t').count() =>
                {
                    Some("text/tab-separated-values")
                },
                _ => Some("text/plain"),
            }
        }
    } else {
        None
    }
}

/// Consumes the same JSON tokens as Value without retaining strings or
/// containers.
struct JsonProbe;

impl<'de> serde::Deserialize<'de> for JsonProbe {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> DeVisitor<'de> for Visitor {
            type Value = JsonProbe;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON")
            }

            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(JsonProbe)
            }

            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(JsonProbe)
            }

            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(JsonProbe)
            }

            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                // deserialize_any still performs serde_json's finite-number parsing.
                Ok(JsonProbe)
            }

            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(JsonProbe)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(JsonProbe)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
                while sequence.next_element::<JsonProbe>()?.is_some() {}
                Ok(JsonProbe)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> Result<Self::Value, A::Error> {
                while object.next_key::<JsonProbe>()?.is_some() {
                    object.next_value::<JsonProbe>()?;
                }
                Ok(JsonProbe)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

fn office_zip_media(bytes: &[u8]) -> Option<&'static str> {
    let directory = zip_directory(bytes, bytes.len() as u64)?;
    let start = usize::try_from(directory.offset).ok()?;
    let end = start.checked_add(usize::try_from(directory.length).ok()?)?;
    let bytes = bytes.get(start..end)?;
    let mut position = 0_usize;
    let mut probe = ZipMediaProbe::default();
    for _ in 0..directory.entries {
        let header = bytes.get(position..position.checked_add(46)?)?;
        let entry = zip_entry(header)?;
        let name_start = position.checked_add(46)?;
        let name_end = name_start.checked_add(usize::from(entry.name_bytes))?;
        probe.observe(bytes.get(name_start..name_end)?);
        position = position.checked_add(entry.total_bytes as usize)?;
    }
    (position == bytes.len()).then(|| probe.media_type()).flatten()
}

/// A ZIP central-directory location extracted from the bounded archive tail.
#[derive(Clone, Copy, Debug)]
pub struct ZipDirectory {
    /// Absolute byte offset of the central directory.
    pub offset: u64,
    /// Number of directory bytes.
    pub length: u64,
    /// Number of file entries.
    pub entries: u16,
}

/// Reads the EOCD from at most the final 65,557 bytes of an archive.
/// Returns no directory for malformed, multi-volume or ZIP64 containers.
#[must_use]
pub fn zip_directory(tail: &[u8], file_size: u64) -> Option<ZipDirectory> {
    const EOCD: usize = 22;
    let base = file_size.checked_sub(tail.len() as u64)?;
    for offset in (0..=tail.len().checked_sub(EOCD)?).rev() {
        let end = &tail[offset..];
        if !end.starts_with(b"PK\x05\x06") {
            continue;
        }
        let number = |position: usize| u16::from_le_bytes([end[position], end[position + 1]]);
        if number(4) != 0
            || number(6) != 0
            || number(8) != number(10)
            || usize::from(number(20)) + EOCD != end.len()
            || number(10) == u16::MAX
        {
            continue;
        }
        let length = u64::from(u32::from_le_bytes(end[12..16].try_into().ok()?));
        let directory_offset = u64::from(u32::from_le_bytes(end[16..20].try_into().ok()?));
        if directory_offset.checked_add(length)? != base.checked_add(offset as u64)? {
            continue;
        }
        return Some(ZipDirectory {
            offset: directory_offset,
            length,
            entries: number(10),
        });
    }
    None
}

/// Bounded information needed to skip one central-directory member.
#[derive(Clone, Copy, Debug)]
pub struct ZipEntry {
    /// Name bytes following the 46-byte header.
    pub name_bytes: u16,
    /// Total bytes including header, name, extra data and comment.
    pub total_bytes: u32,
}

/// Validates one exact 46-byte central-directory header.
#[must_use]
pub fn zip_entry(header: &[u8]) -> Option<ZipEntry> {
    if header.len() != 46 || !header.starts_with(b"PK\x01\x02") {
        return None;
    }
    let number = |position: usize| u16::from_le_bytes([header[position], header[position + 1]]);
    if number(34) != 0 {
        return None;
    }
    let name_bytes = number(28);
    Some(ZipEntry {
        name_bytes,
        total_bytes: 46 + u32::from(name_bytes) + u32::from(number(30)) + u32::from(number(32)),
    })
}

/// Format evidence from parsed ZIP entry names, independent of file data.
#[derive(Clone, Copy, Debug, Default)]
pub struct ZipMediaProbe {
    content_types: bool,
    families: u8,
}

impl ZipMediaProbe {
    /// Records one name from a validated central-directory entry.
    pub fn observe(&mut self, name: &[u8]) {
        match name {
            b"[Content_Types].xml" => self.content_types = true,
            b"xl/workbook.xml" => self.families |= 1,
            b"word/document.xml" => self.families |= 2,
            b"ppt/presentation.xml" => self.families |= 4,
            _ => {},
        }
    }

    /// Identifies a single Office family only when package metadata also
    /// exists.
    #[must_use]
    pub const fn media_type(&self) -> Option<&'static str> {
        match (self.content_types, self.families) {
            (true, 1) => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
            (true, 2) => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
            (true, 4) => Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Pure source format evidence is structural and never changes native
    //! bytes.

    use crate::{
        detect_media_type,
        source_media_matches,
        validate_source_filename,
        validate_source_property,
    };

    #[test]
    fn produced_sources_require_direct_bounded_property_identifiers() {
        for property in ["document", "_", "_Document42", &"x".repeat(64)] {
            assert!(validate_source_property(property).is_ok(), "{property}");
        }
        for property in [
            "",
            "/properties/document",
            "/properties/attachments/items/properties/document",
            "attachments.document",
            "attachments/document",
            "document-name",
            "1document",
            "é",
            "document\0",
            &"x".repeat(65),
        ] {
            assert!(validate_source_property(property).is_err(), "{property}");
        }
    }

    #[test]
    fn archive_payload_names_do_not_impersonate_an_office_package() {
        let fake = b"PK\x03\x04junk xl/workbook.xml [Content_Types].xml";
        assert_eq!(detect_media_type(fake), Some("application/zip"));
        let workbook = include_bytes!("../tests/testdata/workbook.xlsx");
        assert_eq!(
            detect_media_type(workbook),
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        );
        assert_eq!(
            detect_media_type(&workbook[..workbook.len() - 1]),
            Some("application/zip")
        );
    }

    #[test]
    fn filename_bound_counts_utf8_bytes_and_preserves_original_metadata() {
        assert!(validate_source_filename(&format!("{}.pdf", "é".repeat(125))).is_ok());
        assert!(validate_source_filename(&format!("{}.pdf", "é".repeat(126))).is_err());
        assert!(validate_source_filename("invoice\tcopy.pdf").is_ok());
        for invalid in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(validate_source_filename(invalid).is_err());
        }
    }

    #[test]
    fn source_media_constraints_match_case_and_exact_major_type() {
        assert!(source_media_matches("APPLICATION/PDF", "application/pdf"));
        assert!(source_media_matches("Image/*", "image/png"));
        assert!(!source_media_matches("image/*", "images/png"));
        assert!(!source_media_matches("image/*", "image/"));
        assert!(!source_media_matches("image/png", "image/jpeg"));
    }
    // The non-retaining visitor preserves complete/truncated Value parsing.
    #[test]
    fn json_media_probe_preserves_value_parser_acceptance_and_error_categories() {
        let mut samples = vec![
            br#"{"a":[null,true,1,1.5,"text"],"a":2}"#.to_vec(),
            br#"{"escaped":"\uD83D\uDE00\n"}"#.to_vec(),
            br#"["\x"]"#.to_vec(),
            br#"["\uD800"]"#.to_vec(),
            br"[1e400]".to_vec(),
            br"[18446744073709551616]".to_vec(),
            br"[1] trailing".to_vec(),
            br#"{"missing" 1}"#.to_vec(),
        ];
        for depth in [126, 127, 128, 129] {
            samples.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)).into_bytes());
        }
        for bytes in samples {
            for length in 0..=bytes.len() {
                let input = &bytes[..length];
                let expected = serde_json::from_slice::<serde_json::Value>(input)
                    .map(|_| ())
                    .map_err(|error| error.classify());
                let actual = serde_json::from_slice::<super::JsonProbe>(input)
                    .map(|_| ())
                    .map_err(|error| error.classify());
                assert_eq!(actual, expected, "input {input:?}");
            }
        }
        assert_eq!(detect_media_type(br#"{"a":[1,2]}"#), Some("application/json"));
        assert_eq!(detect_media_type(br#"{"a":[1,"#), Some("application/json"));
        assert_eq!(detect_media_type(br"[1e400]"), Some("text/plain"));
    }
}

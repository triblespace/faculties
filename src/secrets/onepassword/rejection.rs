//! Privacy-safe diagnostics: closed static vocabulary, optional numeric errno,
//! and no retained source error, user text, source bytes, or filesystem path.
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtectedImportStage {
    Parse,
    DestinationObservation,
    Encryption,
    ReceiptVerification,
    Preparation,
}
impl ProtectedImportStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::DestinationObservation => "destination_observation",
            Self::Encryption => "encryption",
            Self::ReceiptVerification => "receipt_verification",
            Self::Preparation => "preparation",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtectedImportCode {
    ArchiveByteLimit,
    ArchiveEntryLimit,
    MemberByteLimit,
    ExpandedByteLimit,
    ItemCountLimit,
    ProjectedByteLimit,
    InvalidFormat,
    ZipDirectory,
    ZipArchive,
    ZipLayout,
    ZipMember,
    InvalidMemberPath,
    UnsafeMember,
    ZipIntegrity,
    MemberSizeMismatch,
    MissingAttributes,
    UnsupportedVersion,
    MissingData,
    MalformedJson,
    InvalidStructure,
    DestinationUnavailable,
    EncryptionFailed,
    ReceiptRejected,
    PreparationFailed,
}
impl ProtectedImportCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ArchiveByteLimit => "archive_byte_limit",
            Self::ArchiveEntryLimit => "archive_entry_limit",
            Self::MemberByteLimit => "member_byte_limit",
            Self::ExpandedByteLimit => "expanded_byte_limit",
            Self::ItemCountLimit => "item_count_limit",
            Self::ProjectedByteLimit => "projected_byte_limit",
            Self::InvalidFormat => "invalid_format",
            Self::ZipDirectory => "zip_directory",
            Self::ZipArchive => "zip_archive",
            Self::ZipLayout => "zip_layout",
            Self::ZipMember => "zip_member",
            Self::InvalidMemberPath => "invalid_member_path",
            Self::UnsafeMember => "unsafe_member",
            Self::ZipIntegrity => "zip_integrity",
            Self::MemberSizeMismatch => "member_size_mismatch",
            Self::MissingAttributes => "missing_attributes",
            Self::UnsupportedVersion => "unsupported_version",
            Self::MissingData => "missing_data",
            Self::MalformedJson => "malformed_json",
            Self::InvalidStructure => "invalid_structure",
            Self::DestinationUnavailable => "destination_unavailable",
            Self::EncryptionFailed => "encryption_failed",
            Self::ReceiptRejected => "receipt_rejected",
            Self::PreparationFailed => "preparation_failed",
        }
    }
}

/// A rejection before import publication, distinct from uncertain publication
/// (which retains its receipt). Display/Debug contain only this static vocabulary
/// and, if available, a positive OS error number, never the underlying error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtectedImportRejection {
    pub stage: ProtectedImportStage,
    pub code: ProtectedImportCode,
    pub os_error_code: Option<i32>,
}
impl ProtectedImportRejection {
    pub(crate) fn from_error(
        stage: ProtectedImportStage,
        code: ProtectedImportCode,
        error: &anyhow::Error,
    ) -> Self {
        let os_error_code = error.chain().find_map(|cause| {
            cause
                .downcast_ref::<Self>()
                .and_then(|error| error.os_error_code)
                .or_else(|| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::raw_os_error)
                        .filter(|code| *code > 0)
                })
        });
        Self {
            stage,
            code,
            os_error_code,
        }
    }

    pub(crate) fn parse(error: anyhow::Error) -> Self {
        let code = error
            .downcast_ref::<ParseError>()
            .map_or(ProtectedImportCode::InvalidFormat, |error| error.code);
        Self::from_error(ProtectedImportStage::Parse, code, &error)
    }

    pub(crate) fn preparation(error: anyhow::Error) -> anyhow::Error {
        if let Some(rejection) = error.downcast_ref::<Self>() {
            return (*rejection).into();
        }
        Self::from_error(
            ProtectedImportStage::Preparation,
            ProtectedImportCode::PreparationFailed,
            &error,
        )
        .into()
    }
}
impl fmt::Display for ProtectedImportRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "protected import rejected: {}/{}",
            self.stage.as_str(),
            self.code.as_str()
        )
    }
}
impl std::error::Error for ProtectedImportRejection {}

// Ordinary parser callers retain the existing static human-readable text.
// Protected callers consume the typed code instead of matching error strings.
#[derive(Debug)]
struct ParseError {
    code: ProtectedImportCode,
    message: &'static str,
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for ParseError {}

pub(super) fn parse_error(code: ProtectedImportCode, message: &'static str) -> anyhow::Error {
    ParseError { code, message }.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_refusals_have_distinct_static_reasons_without_input_echo() {
        use super::super::{tests::zip, ImportLimits, OnePasswordArchive};
        let cases = [
            (
                b"SYNTHETIC-PRIVATE-not-a-zip".to_vec(),
                ProtectedImportCode::ZipDirectory,
            ),
            (
                zip(&[("export.data", br#"{"accounts":[]}"#)]),
                ProtectedImportCode::MissingAttributes,
            ),
            (
                zip(&[("export.attributes", br#"{"version":99}"#)]),
                ProtectedImportCode::UnsupportedVersion,
            ),
            (
                zip(&[("export.attributes", br#"{"version":3}"#)]),
                ProtectedImportCode::MissingData,
            ),
            (
                zip(&[("export.attributes", br#"{"SYNTHETIC-PRIVATE-invalid-json"#)]),
                ProtectedImportCode::MalformedJson,
            ),
            (
                zip(&[
                    ("export.attributes", br#"{"version":3}"#),
                    (
                        "export.data",
                        br#"{"accounts":"SYNTHETIC-PRIVATE-invalid-structure"}"#,
                    ),
                ]),
                ProtectedImportCode::InvalidStructure,
            ),
            (
                zip(&[("../SYNTHETIC-PRIVATE-unsafe-path", b"private")]),
                ProtectedImportCode::UnsafeMember,
            ),
        ];
        for (bytes, expected) in cases {
            let rejection = ProtectedImportRejection::parse(
                OnePasswordArchive::parse(&bytes, ImportLimits::default())
                    .err()
                    .unwrap(),
            );
            assert_eq!(rejection.stage, ProtectedImportStage::Parse);
            assert_eq!(rejection.code, expected);
            assert_eq!(rejection.os_error_code, None);
            assert!(!format!("{rejection:?}: {rejection}").contains("SYNTHETIC-PRIVATE"));
        }
    }

    #[test]
    fn static_rejections_keep_numeric_errno_but_no_source_or_sensitive_context() {
        let error = anyhow::Error::new(std::io::Error::from_raw_os_error(24))
            .context("SYNTHETIC-PRIVATE-source/path-and-value");
        for (stage, code) in [
            (
                ProtectedImportStage::DestinationObservation,
                ProtectedImportCode::DestinationUnavailable,
            ),
            (
                ProtectedImportStage::Encryption,
                ProtectedImportCode::EncryptionFailed,
            ),
            (
                ProtectedImportStage::ReceiptVerification,
                ProtectedImportCode::ReceiptRejected,
            ),
        ] {
            let rejection = ProtectedImportRejection::from_error(stage, code, &error);
            assert_eq!(rejection.os_error_code, Some(24));
            assert!(!rejection.to_string().contains("SYNTHETIC-PRIVATE"));
            assert!(!format!("{rejection:?}").contains("SYNTHETIC-PRIVATE"));
            assert!(std::error::Error::source(&rejection).is_none());
            let propagated = ProtectedImportRejection::preparation(
                anyhow::Error::new(rejection).context("SYNTHETIC-PRIVATE-outer-context"),
            );
            assert_eq!(
                propagated.downcast_ref::<ProtectedImportRejection>(),
                Some(&rejection)
            );
            assert!(!format!("{propagated:#}").contains("SYNTHETIC-PRIVATE"));
        }
        let no_errno = anyhow::anyhow!("SYNTHETIC-PRIVATE-untyped-error");
        assert_eq!(
            ProtectedImportRejection::from_error(
                ProtectedImportStage::Encryption,
                ProtectedImportCode::EncryptionFailed,
                &no_errno,
            )
            .os_error_code,
            None
        );
    }
}

//! Errors raised while reading an incoming document.

/// Convenience alias for this crate's results.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The document does not parse as the format it claims to be.
    #[error("malformed {format} document: {detail}")]
    Malformed {
        format: &'static str,
        detail: String,
    },

    /// The document parses, but uses a variant we do not implement.
    #[error("unsupported {format} document: {detail}")]
    Unsupported {
        format: &'static str,
        detail: String,
    },

    /// The document format is not one we accept at all.
    #[error("unrecognised document format")]
    UnknownFormat,

    /// A document with no pages cannot be printed.
    #[error("document contains no pages")]
    Empty,

    /// Something went wrong inside the PDF library.
    #[error("pdf error: {0}")]
    Pdf(#[from] lopdf::Error),
}

impl Error {
    /// Whether the Client is at fault, i.e. whether this maps to a
    /// `client-error-document-format-error` rather than an internal error.
    pub fn is_client_fault(&self) -> bool {
        match self {
            Error::Malformed { .. }
            | Error::Unsupported { .. }
            | Error::UnknownFormat
            | Error::Empty => true,
            // lopdf fails both on broken input and on our own mistakes; a
            // parse failure is far more likely, so blame the document.
            Error::Pdf(_) => true,
        }
    }

    pub(crate) fn malformed(format: &'static str, detail: impl Into<String>) -> Self {
        Error::Malformed {
            format,
            detail: detail.into(),
        }
    }

    pub(crate) fn unsupported(format: &'static str, detail: impl Into<String>) -> Self {
        Error::Unsupported {
            format,
            detail: detail.into(),
        }
    }
}

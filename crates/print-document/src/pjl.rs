//! Strips the PJL envelope some print paths wrap a document in.
//!
//! Printer Job Language is a plain-text control channel: a Universal Exit
//! Language escape, some `@PJL` commands, `@PJL ENTER LANGUAGE = <name>`, then
//! the document itself, then another UEL escape. Nothing in IPP asks for it,
//! but drivers that were built for a port-9100 world sometimes send it anyway,
//! so it is unwrapped rather than rejected.
//!
//! The language name is returned instead of being checked, because what comes
//! out is sniffed with [`Format::sniff`](crate::Format::sniff) like any other
//! input — a driver that mislabels its own payload is then still handled.

/// The Universal Exit Language escape sequence that brackets a PJL job.
pub const UEL: &[u8] = b"\x1B%-12345X";

/// What was found inside a PJL envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope<'a> {
    /// The name given by `@PJL ENTER LANGUAGE`, upper-cased, e.g. `PDF`.
    pub language: String,
    /// The document itself.
    pub data: &'a [u8],
}

/// Whether `data` looks like a PJL job.
pub fn is_pjl(data: &[u8]) -> bool {
    data.starts_with(UEL)
}

/// Unwraps a PJL job, returning `None` if `data` is not one.
///
/// A job that is PJL but carries no `ENTER LANGUAGE` command also yields
/// `None`: there is no document in it to print.
pub fn unwrap(data: &[u8]) -> Option<Envelope<'_>> {
    if !is_pjl(data) {
        return None;
    }

    const ENTER: &[u8] = b"@PJL ENTER LANGUAGE";

    // Commands are newline-separated text, so scan line by line until the one
    // that hands over to a page-description language.
    let mut offset = 0usize;
    let mut language = None;
    while offset < data.len() {
        let line_end = data[offset..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| offset + i)
            .unwrap_or(data.len());
        let line = &data[offset..line_end];

        if let Some(start) = find(line, ENTER) {
            let rest = &line[start + ENTER.len()..];
            // ` = PDF`, `=PDF`, and a trailing CR all occur in the wild.
            let name = rest
                .iter()
                .skip_while(|&&b| b == b' ' || b == b'=' || b == b'\t')
                .take_while(|&&b| !b.is_ascii_whitespace())
                .copied()
                .collect::<Vec<u8>>();
            language = Some(String::from_utf8_lossy(&name).to_uppercase());
            offset = (line_end + 1).min(data.len());
            break;
        }

        offset = line_end + 1;
    }

    let language = language?;
    let body = &data[offset..];
    // The closing UEL, if present, is not part of the document.
    let end = find(body, UEL).unwrap_or(body.len());

    Some(Envelope {
        language,
        data: &body[..end],
    })
}

/// First occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(language: &str, body: &[u8]) -> Vec<u8> {
        let mut out = UEL.to_vec();
        out.extend_from_slice(b"@PJL SET RESOLUTION=600\n");
        out.extend_from_slice(format!("@PJL ENTER LANGUAGE = {language}\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(UEL);
        out
    }

    #[test]
    fn a_wrapped_document_comes_back_whole() {
        let wrapped = wrap("PDF", b"%PDF-1.7\nbody\n");
        let envelope = unwrap(&wrapped).unwrap();
        assert_eq!(envelope.language, "PDF");
        assert_eq!(envelope.data, b"%PDF-1.7\nbody\n");
    }

    #[test]
    fn the_language_is_read_whatever_the_spacing() {
        for header in ["@PJL ENTER LANGUAGE=PDF", "@PJL ENTER LANGUAGE = pdf\r"] {
            let mut bytes = UEL.to_vec();
            bytes.extend_from_slice(header.as_bytes());
            bytes.push(b'\n');
            bytes.extend_from_slice(b"DATA");
            let envelope = unwrap(&bytes).unwrap();
            assert_eq!(envelope.language, "PDF", "{header}");
            assert_eq!(envelope.data, b"DATA");
        }
    }

    #[test]
    fn other_languages_are_unwrapped_too_and_left_to_the_sniffer() {
        let wrapped = wrap("POSTSCRIPT", b"%!PS\n");
        let envelope = unwrap(&wrapped).unwrap();
        assert_eq!(envelope.language, "POSTSCRIPT");
        assert_eq!(envelope.data, b"%!PS\n");
    }

    #[test]
    fn a_missing_closing_escape_is_tolerated() {
        let mut bytes = UEL.to_vec();
        bytes.extend_from_slice(b"@PJL ENTER LANGUAGE = PDF\n");
        bytes.extend_from_slice(b"%PDF-1.7");
        assert_eq!(unwrap(&bytes).unwrap().data, b"%PDF-1.7");
    }

    #[test]
    fn documents_that_are_not_pjl_are_left_alone() {
        assert!(unwrap(b"%PDF-1.7\n").is_none());
        assert!(unwrap(b"").is_none());
        assert!(!is_pjl(b"%PDF-1.7"));
    }

    #[test]
    fn pjl_without_a_document_yields_nothing() {
        let mut bytes = UEL.to_vec();
        bytes.extend_from_slice(b"@PJL INFO STATUS\n");
        bytes.extend_from_slice(UEL);
        assert!(unwrap(&bytes).is_none());
    }
}

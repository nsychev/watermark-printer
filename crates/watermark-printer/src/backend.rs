//! Job processing.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Local;
use ippserver::{Accepted, Backend, JobRequest, Rejection, StatusCode};
use print_document::{Format, Media, pjl};
use print_watermark::{Layout, Stamp};
use universal_print::{Job, Sink};

use crate::teams::TeamIds;

#[derive(Debug)]
pub struct Plan {
    /// The mark to put on the page.
    team: String,
    /// Document format.
    format: Format,
    /// Whether the document arrived wrapped in PJL.
    wrapped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Applies to PDF and JPEG.
    pub document_bytes: usize,
    /// Applies to PWG Raster.
    pub raster_bytes: usize,
}

impl Limits {
    /// What the transport must allow through, being the larger of the two.
    pub fn transport_bytes(&self) -> usize {
        self.document_bytes.max(self.raster_bytes)
    }

    /// The limit that applies to `format`.
    pub fn for_format(&self, format: Format) -> usize {
        match format {
            Format::PwgRaster => self.raster_bytes,
            Format::Pdf | Format::Jpeg => self.document_bytes,
        }
    }
}

pub struct WatermarkBackend {
    teams: TeamIds,
    layout: Layout,
    sink: Sink,
    /// Where stamped documents are kept, if anywhere.
    storage: Option<PathBuf>,
    /// The sheet an image is fitted to when the Client asks for nothing.
    default_media: Media,
    limits: Limits,
}

impl WatermarkBackend {
    pub fn new(
        teams: TeamIds,
        layout: Layout,
        sink: Sink,
        storage: Option<PathBuf>,
        default_media: Media,
        limits: Limits,
    ) -> Self {
        WatermarkBackend {
            limits,
            teams,
            layout,
            sink,
            storage,
            default_media,
        }
    }

    /// Unwraps a PJL envelope, if there is one.
    fn body(document: &[u8], wrapped: bool) -> &[u8] {
        if wrapped {
            pjl::unwrap(document).map(|e| e.data).unwrap_or(document)
        } else {
            document
        }
    }

    /// The sheet to fit an image to, honouring what the Client asked for.
    fn media_for(&self, request: &JobRequest) -> Media {
        request
            .media()
            .and_then(ippserver::media::find)
            .map(|size| {
                let (width, height) = size.points();
                Media::new(width, height)
            })
            .unwrap_or(self.default_media)
    }

    /// Writes a copy of what was printed, best effort.
    fn keep(&self, request: &JobRequest, bytes: &[u8]) -> Option<PathBuf> {
        let directory = self.storage.as_ref()?.join(request.origin.to_string());
        if let Err(error) = std::fs::create_dir_all(&directory) {
            tracing::warn!(%error, directory = %directory.display(), "could not create the storage directory");
            return None;
        }

        let path = directory.join(format!(
            "{}-{}.pdf",
            Local::now().format("%Y%m%dT%H%M%S"),
            request.uuid
        ));
        match std::fs::write(&path, bytes) {
            Ok(()) => Some(path),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "could not keep a copy of the job");
                None
            }
        }
    }
}

impl WatermarkBackend {
    /// [`Backend::validate`] will log any `Rejection` this method returns.
    fn check(&self, request: &JobRequest) -> Result<Accepted<Plan>, Rejection> {
        // Who is this, and may they print at all?
        let team = match self.teams.lookup(request.origin) {
            Ok(Some(team)) => team,
            Ok(None) => {
                return Err(Rejection::new(
                    StatusCode::ClientErrorNotAuthorized,
                    format!("{} is not mapped to a team", request.origin),
                ));
            }
            Err(error) => {
                tracing::error!(%error, "the team id script failed");
                return Err(Rejection::new(
                    StatusCode::ServerErrorInternalError,
                    "could not work out which team this is",
                ));
            }
        };

        let wrapped = pjl::is_pjl(&request.document);
        if wrapped {
            // Legacy clients wrap the document in a PJL envelope, should not happen.
            let language = pjl::unwrap(&request.document)
                .map(|envelope| envelope.language)
                .unwrap_or_default();
            tracing::info!(%language, "job arrived wrapped in PJL");
        }
        let body = Self::body(&request.document, wrapped);
        if body.is_empty() {
            return Err(Rejection::bad_document("the document is empty"));
        }

        let format = Format::sniff(body)
            .or_else(|| Format::from_mime(&request.format))
            .ok_or_else(|| {
                Rejection::new(
                    StatusCode::ClientErrorDocumentFormatNotSupported,
                    "the document is not a PDF, a JPEG or PWG Raster",
                )
            })?;

        let limit = self.limits.for_format(format);
        if body.len() > limit {
            return Err(Rejection::new(
                StatusCode::ClientErrorRequestEntityTooLong,
                format!(
                    "the document is {:.1} MB and the limit for {} is {:.0} MB",
                    body.len() as f64 / (1024.0 * 1024.0),
                    format.mime(),
                    limit as f64 / (1024.0 * 1024.0),
                ),
            ));
        }

        let pages = print_document::page_count(body, format).map_err(|error| {
            if error.is_client_fault() {
                Rejection::bad_document(error.to_string())
            } else {
                Rejection::new(StatusCode::ServerErrorInternalError, error.to_string())
            }
        })?;

        Ok(Accepted {
            impressions: pages as i32,
            format: Some(format.mime().to_string()),
            plan: Plan {
                team,
                format,
                wrapped,
            },
        })
    }
}

#[async_trait::async_trait]
impl Backend for WatermarkBackend {
    type Plan = Plan;

    async fn validate(&self, request: &JobRequest) -> Result<Accepted<Plan>, Rejection> {
        self.check(request).inspect_err(|rejection| {
            tracing::warn!(
                job = request.id,
                origin = %request.origin,
                status = ?rejection.status,
                format = %request.format,
                bytes = request.document.len(),
                "refused the job: {}",
                rejection.message,
            );
        })
    }

    async fn print(&self, request: JobRequest, plan: Plan) -> Result<(), String> {
        let media = self.media_for(&request);
        let layout = self.layout;
        let stamp_team = plan.team.clone();
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let document = request.document.clone();
        let wrapped = plan.wrapped;
        let format = plan.format;

        // Parsing and re-rendering a large document is pure CPU work, so it
        // does not belong on a runtime worker thread.
        let rendered = tokio::task::spawn_blocking(move || {
            let body = Self::body(&document, wrapped);
            let source = print_document::load(body, format, media)
                .map_err(|e| format!("could not read the document: {e}"))?;
            print_watermark::render(
                source,
                &Stamp {
                    team: &stamp_team,
                    timestamp: &timestamp,
                },
                &layout,
                media,
            )
            .map_err(|e| format!("could not mark the document: {e}"))
        })
        .await
        .map_err(|e| format!("the rendering task failed: {e}"))??;

        if rendered.lossy_text {
            tracing::warn!(
                team = %plan.team,
                "the team name contains characters the standard font cannot show"
            );
        }

        let kept = self.keep(&request, &rendered.bytes);

        let name = format!("{} [{}]", request.name, plan.team);
        let job = Job {
            data: &rendered.bytes,
            format: "application/pdf",
            name: &name,
            user: Some(&request.user),
            copies: request.copies() as u32,
            media: request.media(),
            sides: request.sides(),
        };

        self.sink.print(&job).await.map_err(|error| {
            // The kept copy is the thing to look at when a job fails, so say
            // where it is in the same breath.
            match &kept {
                Some(path) => format!("{error} (the marked document is at {})", path.display()),
                None => error.to_string(),
            }
        })?;

        tracing::info!(
            job = request.id,
            team = %plan.team,
            pages = rendered.pages,
            format = format.mime(),
            "printed"
        );
        Ok(())
    }
}

/// Reports what a backend would do, for the startup banner.
pub fn describe(sink: &Sink, storage: Option<&Path>, teams: &TeamIds) -> String {
    format!(
        "sending to {}; teams from {}; {}",
        sink.label(),
        teams.source(),
        match storage {
            Some(path) => format!("keeping copies in {}", path.display()),
            None => "keeping no copies".to_string(),
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    const TEST_LIMITS: Limits = Limits {
        document_bytes: 5 * 1024 * 1024,
        raster_bytes: 50 * 1024 * 1024,
    };

    fn backend(storage: Option<PathBuf>) -> WatermarkBackend {
        backend_with(storage, TEST_LIMITS)
    }

    fn backend_with(storage: Option<PathBuf>, limits: Limits) -> WatermarkBackend {
        WatermarkBackend::new(
            TeamIds::load(None).unwrap(),
            Layout::default(),
            Sink::detect(Some("dry-run"), None, universal_print::DEFAULT_TIMEOUT).unwrap(),
            storage,
            Media::A4,
            limits,
        )
    }

    fn request(origin: &str, document: Vec<u8>) -> JobRequest {
        JobRequest {
            id: 1,
            uuid: Uuid::from_u128(7),
            name: "a job".into(),
            user: "tester".into(),
            origin: origin.parse().unwrap(),
            format: "application/pdf".into(),
            document,
            template: Vec::new(),
        }
    }

    /// A one-page JPEG, which is the cheapest valid document to build.
    fn jpeg() -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08];
        bytes.extend_from_slice(&600u16.to_be_bytes());
        bytes.extend_from_slice(&800u16.to_be_bytes());
        bytes.extend_from_slice(&[3, 0, 0, 0]);
        bytes.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        bytes
    }

    #[tokio::test]
    async fn a_known_address_is_accepted_and_its_pages_counted() {
        let backend = backend(None);
        let accepted = backend
            .validate(&request("10.21.32.43", jpeg()))
            .await
            .unwrap();
        assert_eq!(accepted.impressions, 1);
        assert_eq!(accepted.format.as_deref(), Some("image/jpeg"));
        assert_eq!(accepted.plan.team, "032");
    }

    #[tokio::test]
    async fn an_unmapped_address_is_refused_rather_than_silently_dropped() {
        let backend = backend(None);
        let rejection = backend
            .validate(&request("2001:db8::1", jpeg()))
            .await
            .unwrap_err();
        assert_eq!(rejection.status, StatusCode::ClientErrorNotAuthorized);
        assert!(rejection.message.contains("not mapped"), "{rejection:?}");
    }

    #[tokio::test]
    async fn the_declared_format_does_not_override_what_was_actually_sent() {
        let backend = backend(None);
        // Declared as PDF, actually a JPEG.
        let accepted = backend
            .validate(&request("10.0.1.1", jpeg()))
            .await
            .unwrap();
        assert_eq!(accepted.format.as_deref(), Some("image/jpeg"));
    }

    #[tokio::test]
    async fn a_pjl_wrapped_document_is_unwrapped_before_being_read() {
        let mut wrapped = b"\x1B%-12345X".to_vec();
        wrapped.extend_from_slice(b"@PJL ENTER LANGUAGE = JPEG\n");
        wrapped.extend_from_slice(&jpeg());
        wrapped.extend_from_slice(b"\x1B%-12345X");

        let backend = backend(None);
        let accepted = backend
            .validate(&request("10.0.1.1", wrapped))
            .await
            .unwrap();
        assert!(accepted.plan.wrapped);
        assert_eq!(accepted.format.as_deref(), Some("image/jpeg"));
    }

    #[tokio::test]
    async fn rubbish_and_empty_documents_are_refused_as_the_clients_fault() {
        let backend = backend(None);
        for document in [Vec::new(), b"not a document".to_vec()] {
            let rejection = backend
                .validate(&request("10.0.1.1", document))
                .await
                .unwrap_err();
            assert!(
                matches!(
                    rejection.status,
                    StatusCode::ClientErrorDocumentFormatError
                        | StatusCode::ClientErrorDocumentFormatNotSupported
                ),
                "{rejection:?}"
            );
        }
    }

    #[tokio::test]
    async fn printing_marks_the_document_and_keeps_a_copy() {
        let directory = tempfile::tempdir().unwrap();
        let backend = backend(Some(directory.path().to_path_buf()));

        let request = request("10.21.32.43", jpeg());
        let accepted = backend.validate(&request).await.unwrap();
        backend.print(request, accepted.plan).await.unwrap();

        let kept: Vec<_> = walk(directory.path());
        assert_eq!(kept.len(), 1, "{kept:?}");
        let bytes = std::fs::read(&kept[0]).unwrap();
        assert!(bytes.starts_with(b"%PDF-"), "the copy is not a PDF");
        // The copy is filed under the address it came from.
        assert!(
            kept[0].parent().unwrap().ends_with("10.21.32.43"),
            "{kept:?}"
        );
        // That the mark itself is drawn is print-watermark's business, and
        // its own tests cover it; the content streams here are compressed.
    }

    #[tokio::test]
    async fn the_size_limit_that_applies_is_the_one_for_the_format_that_arrived() {
        // A JPEG just over the document limit is refused...
        let tight = backend_with(
            None,
            Limits {
                // Smaller than the test document, which is a couple of dozen
                // bytes of JPEG markers.
                document_bytes: 8,
                raster_bytes: 50 * 1024 * 1024,
            },
        );
        let rejection = tight
            .validate(&request("10.21.32.43", jpeg()))
            .await
            .unwrap_err();
        assert_eq!(
            rejection.status,
            StatusCode::ClientErrorRequestEntityTooLong
        );
        assert!(rejection.message.contains("image/jpeg"), "{rejection:?}");

        // ...while the far larger raster allowance does not rescue it, and
        // the generous default accepts the same document happily.
        assert!(
            backend(None)
                .validate(&request("10.21.32.43", jpeg()))
                .await
                .is_ok()
        );
    }

    #[test]
    fn the_transport_limit_is_the_larger_of_the_two() {
        assert_eq!(TEST_LIMITS.transport_bytes(), 50 * 1024 * 1024);
        assert_eq!(TEST_LIMITS.for_format(Format::Pdf), 5 * 1024 * 1024);
        assert_eq!(TEST_LIMITS.for_format(Format::Jpeg), 5 * 1024 * 1024);
        assert_eq!(TEST_LIMITS.for_format(Format::PwgRaster), 50 * 1024 * 1024);
    }

    #[tokio::test]
    async fn a_job_prints_even_when_no_copy_can_be_kept() {
        // A path that cannot be created.
        let backend = backend(Some(PathBuf::from("/proc/nonexistent/storage")));
        let request = request("10.21.32.43", jpeg());
        let accepted = backend.validate(&request).await.unwrap();
        assert!(backend.print(request, accepted.plan).await.is_ok());
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            for entry in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    found.push(path);
                }
            }
        }
        found
    }
}

//! Job processing.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local};
use ippserver::{Accepted, Backend, JobRequest, Outcome, Rejection, StatusCode};
use print_document::{Format, Media, pjl};
use print_watermark::{Layout, Stamp};
use universal_print::{Job, Sink};

use crate::teams::TeamIds;

/// How many times a job is offered to the printer before it is given up on.
/// The first attempt is the Client's; the rest follow a deferral.
const MAX_ATTEMPTS: u32 = 6;

/// How long a job waits after each attempt, giving up after [`MAX_ATTEMPTS`].
/// About eight minutes in all, which is long enough to refill the paper.
const DEFER_DELAYS: [Duration; (MAX_ATTEMPTS - 1) as usize] = [
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
];

/// How long a printer that would not take a job is assumed to still be
/// refusing. Within this window a new job is parked without being offered,
/// rather than spending the sink's retries over again to learn the same
/// thing. Deferred jobs are always offered: they are what finds out that the
/// printer is working again.
const REFUSAL_LASTS: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub struct Plan {
    /// The mark to put on the page.
    team: String,
    /// Document format.
    format: Format,
    /// Whether the document arrived wrapped in PJL.
    wrapped: bool,
    /// When the Client handed the job over.
    ///
    /// The mark says this rather than the moment the page is sent, so that a
    /// job held back while the printer was out of paper still says when its
    /// team printed it, and so that re-rendering it produces the same bytes.
    submitted: DateTime<Local>,
}

/// How long to hold a job that has been offered `attempt` times, or `None`
/// when it has had its chances.
fn defer_for(attempt: u32) -> Option<Duration> {
    DEFER_DELAYS.get(attempt.checked_sub(1)? as usize).copied()
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
    /// When the printer last would not take a job.
    refused_at: Mutex<Option<Instant>>,
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
            refused_at: Mutex::new(None),
        }
    }

    /// Whether the printer turned a job away recently enough that the next
    /// one is better off waiting its turn than finding out the hard way.
    fn still_refusing(&self) -> bool {
        self.refused_at
            .lock()
            .expect("the refusal clock is poisoned")
            .is_some_and(|at| at.elapsed() < REFUSAL_LASTS)
    }

    fn note_refusal(&self) {
        *self
            .refused_at
            .lock()
            .expect("the refusal clock is poisoned") = Some(Instant::now());
    }

    fn note_success(&self) {
        *self
            .refused_at
            .lock()
            .expect("the refusal clock is poisoned") = None;
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
    ///
    /// The name comes from the job, not the clock, so a job offered more than
    /// once leaves one copy rather than one per attempt.
    fn keep(&self, request: &JobRequest, plan: &Plan, bytes: &[u8]) -> Option<PathBuf> {
        let directory = self.storage.as_ref()?.join(request.origin.to_string());
        if let Err(error) = std::fs::create_dir_all(&directory) {
            tracing::warn!(%error, directory = %directory.display(), "could not create the storage directory");
            return None;
        }

        let path = directory.join(format!(
            "{}-{}.pdf",
            plan.submitted.format("%Y%m%dT%H%M%S"),
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
                submitted: Local::now(),
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

    async fn print(&self, request: &JobRequest, plan: &Plan, attempt: u32) -> Outcome {
        // The printer turned a job away a moment ago, so this one would only
        // learn the same thing the slow way. Park it without rendering it.
        if attempt == 1
            && self.still_refusing()
            && let Some(after) = defer_for(attempt)
        {
            tracing::info!(
                job = request.id,
                "the printer was not taking jobs; waiting {after:?} before offering this one"
            );
            return Outcome::Deferred {
                reason: "the printer was not taking jobs".into(),
                after,
            };
        }

        let media = self.media_for(request);
        let layout = self.layout;
        let stamp_team = plan.team.clone();
        let timestamp = plan.submitted.format("%Y-%m-%d %H:%M:%S").to_string();
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
        .map_err(|e| format!("the rendering task failed: {e}"));

        // A document that cannot be marked will not be marked next time
        // either, so there is nothing to wait for.
        let rendered = match rendered.and_then(|result| result) {
            Ok(rendered) => rendered,
            Err(error) => return Outcome::Failed(error),
        };

        if rendered.lossy_text {
            tracing::warn!(
                team = %plan.team,
                "the team name contains characters the standard font cannot show"
            );
        }

        let kept = self.keep(request, plan, &rendered.bytes);

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

        let error = match self.sink.print(&job).await {
            Ok(()) => {
                self.note_success();
                tracing::info!(
                    job = request.id,
                    team = %plan.team,
                    pages = rendered.pages,
                    attempt,
                    format = format.mime(),
                    "printed"
                );
                return Outcome::Printed;
            }
            Err(error) => error,
        };

        // The kept copy is the thing to look at when a job fails, so say
        // where it is in the same breath.
        let detail = match &kept {
            Some(path) => format!("{error} (the marked document is at {})", path.display()),
            None => error.to_string(),
        };

        // Only the printer's own "ask me later" answers are worth waiting
        // out. The sink has already ridden out a short busy spell by the
        // time one reaches here, so the trouble is the sort that needs a
        // person: out of paper, jammed, or switched off.
        if !error.is_retryable() {
            return Outcome::Failed(detail);
        }

        self.note_refusal();
        match defer_for(attempt) {
            Some(after) => Outcome::Deferred {
                reason: detail,
                after,
            },
            None => Outcome::Failed(format!("{detail}; gave up after {attempt} attempts")),
        }
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        sink_backend(
            Sink::detect(Some("dry-run"), None, universal_print::DEFAULT_TIMEOUT).unwrap(),
            storage,
            limits,
        )
    }

    fn sink_backend(sink: Sink, storage: Option<PathBuf>, limits: Limits) -> WatermarkBackend {
        WatermarkBackend::new(
            TeamIds::load(None).unwrap(),
            Layout::default(),
            sink,
            storage,
            Media::A4,
            limits,
        )
    }

    /// A printer that answers every request with `server-error-busy`, which
    /// is what a printer with a full queue does.
    fn busy_printer() -> (String, Arc<AtomicUsize>) {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let asked = Arc::new(AtomicUsize::new(0));

        let counter = Arc::clone(&asked);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                counter.fetch_add(1, Ordering::SeqCst);

                // Read what the client has to say before answering, so it is
                // never cut off mid-write.
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut scratch = [0u8; 8192];
                while let Ok(read) = stream.read(&mut scratch) {
                    if read < scratch.len() {
                        break;
                    }
                }

                // version 2.0, server-error-busy, request id, and the two
                // operation attributes every response must carry.
                let mut body = vec![0x02, 0x00, 0x05, 0x07, 0, 0, 0, 1, 0x01];
                for (tag, name, value) in [
                    (0x47u8, "attributes-charset", "utf-8"),
                    (0x48u8, "attributes-natural-language", "en-us"),
                ] {
                    body.push(tag);
                    body.extend_from_slice(&(name.len() as u16).to_be_bytes());
                    body.extend_from_slice(name.as_bytes());
                    body.extend_from_slice(&(value.len() as u16).to_be_bytes());
                    body.extend_from_slice(value.as_bytes());
                }
                body.push(0x03);

                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/ipp\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });

        (format!("ipp://127.0.0.1:{port}/ipp/print"), asked)
    }

    /// A backend pointed at a printer that will not take anything. The sink's
    /// own retries are off so the test does not sit through them; by the time
    /// a job is deferred in earnest they will have been spent anyway.
    fn busy_backend() -> (WatermarkBackend, Arc<AtomicUsize>) {
        let (uri, asked) = busy_printer();
        let sink = Sink::detect(Some(&uri), None, universal_print::DEFAULT_TIMEOUT)
            .unwrap()
            .with_retry(universal_print::Retry::none());
        (sink_backend(sink, None, TEST_LIMITS), asked)
    }

    async fn plan_for(backend: &WatermarkBackend, request: &JobRequest) -> Plan {
        backend.validate(request).await.unwrap().plan
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
        assert!(matches!(
            backend.print(&request, &accepted.plan, 1).await,
            Outcome::Printed
        ));

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
        assert!(matches!(
            backend.print(&request, &accepted.plan, 1).await,
            Outcome::Printed
        ));
    }

    #[test]
    fn a_job_is_held_a_little_longer_each_time_and_then_no_longer() {
        assert_eq!(defer_for(1), Some(Duration::from_secs(15)));
        assert_eq!(defer_for(2), Some(Duration::from_secs(30)));
        assert_eq!(defer_for(5), Some(Duration::from_secs(240)));
        // The last attempt has nothing left to wait for.
        assert_eq!(defer_for(MAX_ATTEMPTS), None);
        assert_eq!(defer_for(0), None);

        // Long enough to refill the paper, short enough that nobody is left
        // wondering where their printout went.
        let held: Duration = (1..MAX_ATTEMPTS).filter_map(defer_for).sum();
        assert!(
            held > Duration::from_secs(300) && held < Duration::from_secs(600),
            "{held:?}"
        );
    }

    #[tokio::test]
    async fn a_busy_printer_defers_the_job_rather_than_losing_it() {
        let (backend, _) = busy_backend();
        let request = request("10.21.32.43", jpeg());
        let plan = plan_for(&backend, &request).await;

        match backend.print(&request, &plan, 1).await {
            Outcome::Deferred { reason, after } => {
                assert_eq!(after, Duration::from_secs(15));
                // The reason reaches the Client as job-state-message.
                assert!(reason.contains("Busy"), "{reason}");
            }
            other => panic!("expected the job to be held, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_job_is_given_up_on_once_it_has_had_its_chances() {
        let (backend, _) = busy_backend();
        let request = request("10.21.32.43", jpeg());
        let plan = plan_for(&backend, &request).await;

        match backend.print(&request, &plan, MAX_ATTEMPTS).await {
            Outcome::Failed(error) => assert!(error.contains("gave up"), "{error}"),
            other => panic!("expected the job to be given up on, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_printer_that_just_refused_is_not_asked_again_straight_away() {
        let (backend, asked) = busy_backend();
        let first = request("10.21.32.43", jpeg());
        let second = request("10.0.1.1", jpeg());
        let plan = plan_for(&backend, &first).await;

        // The first job finds out the hard way.
        assert!(matches!(
            backend.print(&first, &plan, 1).await,
            Outcome::Deferred { .. }
        ));
        let after_first = asked.load(Ordering::SeqCst);
        assert!(after_first > 0, "the printer should have been offered it");

        // The next one is parked on what the first one learnt.
        let plan = plan_for(&backend, &second).await;
        assert!(matches!(
            backend.print(&second, &plan, 1).await,
            Outcome::Deferred { .. }
        ));
        assert_eq!(
            asked.load(Ordering::SeqCst),
            after_first,
            "the printer should not have been troubled a second time"
        );

        // A job already waiting is always offered: it is what finds out the
        // printer is working again.
        assert!(matches!(
            backend.print(&second, &plan, 2).await,
            Outcome::Deferred { .. }
        ));
        assert!(asked.load(Ordering::SeqCst) > after_first);
    }

    #[tokio::test]
    async fn a_job_offered_twice_leaves_one_copy_with_one_timestamp() {
        let directory = tempfile::tempdir().unwrap();
        let backend = backend(Some(directory.path().to_path_buf()));

        let request = request("10.21.32.43", jpeg());
        let plan = plan_for(&backend, &request).await;
        backend.print(&request, &plan, 1).await;
        let first = std::fs::read(&walk(directory.path())[0]).unwrap();
        backend.print(&request, &plan, 2).await;

        // The mark says when the job was handed over, not when it was sent,
        // so a job held back does not drift and the copy is rewritten rather
        // than duplicated.
        let kept = walk(directory.path());
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(first, std::fs::read(&kept[0]).unwrap());
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

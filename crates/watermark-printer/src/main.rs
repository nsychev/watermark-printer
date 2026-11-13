//! A virtual printer that marks every job with the watermark and forwards it to the real printer.

mod backend;
mod teams;

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use ippserver::{Printer, PrinterConfig, media, serve};
use print_document::Media;
use print_watermark::Layout;
use tracing_subscriber::EnvFilter;
use universal_print::{DEFAULT_TIMEOUT, Sink};
use uuid::Uuid;

use crate::backend::WatermarkBackend;
use crate::teams::TeamIds;

/// A printer should keep the same UUID across restarts.
const UUID_NAMESPACE: Uuid = Uuid::NAMESPACE_OID;

const MANUFACTURER: &str = "ippserver";
const MAKE_AND_MODEL: &str = "Watermark Printer";

#[derive(Parser, Debug)]
#[command(
    version,
    about = "A virtual printer that watermarks and forwards print jobs"
)]
struct Args {
    #[arg(
        short,
        long,
        default_value = "Watermark Printer",
        help = "Printer name"
    )]
    name: String,

    #[arg(
        short = 'p',
        long,
        default_value = "631",
        help = "Address or port to bind (e.g. 0.0.0.0:631, [::1]:631 or 631)"
    )]
    bind: String,

    #[arg(
        short = 'I',
        long,
        default_value = "auto",
        help = "Where to send marked jobs: auto, an ipp:// URI, lp, lpr, \
                winspooler, sumatra[:path] or command:<template>"
    )]
    destination: String,

    #[arg(long, help = "Printer queue name, for the local destinations")]
    printer: Option<String>,

    #[arg(
        short,
        long,
        help = "Directory to keep a copy of every marked document in"
    )]
    storage: Option<PathBuf>,

    #[arg(short, long, help = "Lua script defining get_team_id(ip_address)")]
    team_id_script: Option<String>,

    #[arg(
        long,
        help = "Refuse a job with more pages than this [default: no limit]"
    )]
    max_pages: Option<i32>,

    #[arg(
        long,
        default_value = "16",
        help = "Refuse a job while this client already has that many queued"
    )]
    max_queued_per_ip: usize,

    #[arg(
        long,
        default_value = "5",
        help = "Refuse a PDF or JPEG larger than this many megabytes"
    )]
    max_document_mb: usize,

    #[arg(
        long,
        default_value = "50",
        help = "Refuse a PWG Raster document larger than this many megabytes"
    )]
    max_raster_mb: usize,

    #[arg(
        long,
        default_value = media::DEFAULT,
        help = "The paper size this printer offers"
    )]
    paper_size: String,

    #[arg(long, help = "Advertise the printer as colour")]
    color: bool,

    #[arg(short, long, help = "Enable verbose logging")]
    verbose: bool,

    #[arg(
        long,
        default_value_t = Layout::default().overlay_stroke_width,
        value_parser = parse_weight,
        help = "Thickness of the watermark outline, as a fraction of the font size"
    )]
    watermark_weight: f64,
}

const MAX_WEIGHT: f64 = 0.2;

/// RUST_LOG will override the default.
fn log_filter(verbose: bool) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_logging(verbose)))
}

fn default_logging(verbose: bool) -> String {
    let level = if verbose { "debug" } else { "info" };
    format!("watermark_printer={level},ippserver={level},universal_print={level}")
}

fn parse_weight(value: &str) -> Result<f64> {
    let weight: f64 = value
        .parse()
        .with_context(|| format!("{value:?} is not a number"))?;
    if !(weight.is_finite() && weight > 0.0 && weight <= MAX_WEIGHT) {
        bail!("the watermark weight must be greater than 0 and at most {MAX_WEIGHT}");
    }
    Ok(weight)
}

/// Accepts `host:port`, a bare port, or a bare address.
fn parse_bind(value: &str) -> Result<SocketAddr> {
    if let Ok(address) = value.parse::<SocketAddr>() {
        return Ok(address);
    }
    if let Ok(port) = value.parse::<u16>() {
        // The unspecified IPv6 address also accepts IPv4 on every platform
        // this runs on, so one listener covers both.
        return Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port));
    }
    if let Ok(address) = value.parse::<IpAddr>() {
        return Ok(SocketAddr::new(address, 631));
    }
    bail!("invalid bind address {value:?}; expected 'address:port', an address, or a port")
}

/// Works out where finished jobs go.
fn build_sink(args: &Args) -> Result<Sink> {
    Sink::detect(
        Some(&args.destination),
        args.printer.as_deref(),
        DEFAULT_TIMEOUT,
    )
    .with_context(|| format!("reading the destination {:?}", args.destination))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(log_filter(args.verbose))
        .init();

    let address = parse_bind(&args.bind)?;

    let layout = Layout {
        overlay_stroke_width: args.watermark_weight,
        ..Layout::default()
    };

    let sheet = *media::find(&args.paper_size).with_context(|| {
        format!(
            "{:?} is not a paper size this printer knows; try one of: {}",
            args.paper_size,
            media::SUPPORTED
                .iter()
                .map(|m| m.name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let (width, height) = sheet.points();

    let teams = TeamIds::load(args.team_id_script.as_deref())?;
    let sink = build_sink(&args)?;

    // Fail at startup rather than one job at a time.
    sink.check("application/pdf")
        .await
        .context("checking the destination printer")?;

    if let Some(storage) = &args.storage {
        std::fs::create_dir_all(storage)
            .with_context(|| format!("creating the storage directory {}", storage.display()))?;
    }

    let banner = backend::describe(&sink, args.storage.as_deref(), &teams);
    let limits = backend::Limits {
        document_bytes: args.max_document_mb.saturating_mul(1024 * 1024),
        raster_bytes: args.max_raster_mb.saturating_mul(1024 * 1024),
    };
    let backend = WatermarkBackend::new(
        teams,
        layout,
        sink,
        args.storage.clone(),
        Media::new(width, height),
        limits,
    );

    let hostname = hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok());

    let config = PrinterConfig {
        name: args.name.clone(),
        info: "".into(),
        manufacturer: MANUFACTURER.into(),
        make_and_model: MAKE_AND_MODEL.into(),
        // Derived from the name to persist between restarts.
        uuid: Uuid::new_v5(&UUID_NAMESPACE, args.name.as_bytes()),
        resolutions: vec![150, 300],
        default_resolution: 300,
        media: vec![sheet],
        default_media: sheet,
        max_copies: 1,
        color: args.color,
        max_pages_per_job: args.max_pages,
        max_queued_per_origin: Some(args.max_queued_per_ip),
        max_document_bytes: limits.transport_bytes(),
        hostname: hostname.clone(),
        ..PrinterConfig::default()
    };
    let path = config.path.clone();

    let printer =
        Printer::start(config, backend).context("the printer configuration is not usable")?;

    tracing::info!("{banner}");
    tracing::info!(
        "listening on {address}; add the printer as ipp://{}:{}{path}",
        hostname.as_deref().unwrap_or("this-host"),
        address.port()
    );

    serve(address, printer)
        .await
        .with_context(|| format!("serving on {address}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_port_listens_on_every_address() {
        let address = parse_bind("6631").unwrap();
        assert_eq!(address.port(), 6631);
        assert!(address.ip().is_unspecified());
    }

    #[test]
    fn explicit_addresses_are_taken_as_given() {
        assert_eq!(
            parse_bind("127.0.0.1:631").unwrap(),
            "127.0.0.1:631".parse().unwrap()
        );
        assert_eq!(
            parse_bind("[::1]:631").unwrap(),
            "[::1]:631".parse().unwrap()
        );
        // A bare address gets the IPP port.
        assert_eq!(parse_bind("10.0.0.5").unwrap().port(), 631);
    }

    #[test]
    fn nonsense_bind_values_are_refused_with_a_useful_message() {
        let error = parse_bind("not an address").unwrap_err().to_string();
        assert!(error.contains("invalid bind address"), "{error}");
        assert!(parse_bind("").is_err());
        // Out of range for a port.
        assert!(parse_bind("99999").is_err());
    }

    /// Parses a command line, pinning the destination to `dry-run` unless the
    /// test is exercising it itself, so no test reaches a real printer.
    fn args_with(extra: &[&str]) -> Args {
        let mut argv = vec!["watermark-printer"];
        if !extra.iter().any(|a| *a == "-I" || *a == "--destination") {
            argv.extend_from_slice(&["--destination", "dry-run"]);
        }
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).unwrap()
    }

    #[test]
    fn the_destination_defaults_to_auto() {
        // With nothing asked for, pick up whatever this machine prints with.
        let args = Args::try_parse_from(["watermark-printer"]).unwrap();
        assert_eq!(args.destination, "auto");
    }

    #[test]
    fn an_ipp_uri_becomes_an_ipp_destination() {
        let args = args_with(&["-I", "ipp://printer.local:631/ipp/print"]);
        let sink = build_sink(&args).unwrap();
        assert!(sink.label().contains("printer.local"), "{}", sink.label());
    }

    #[test]
    fn a_command_template_becomes_a_local_destination() {
        let args = args_with(&["-I", "command:lp {file}"]);
        let sink = build_sink(&args).unwrap();
        assert!(sink.label().contains("lp {file}"), "{}", sink.label());
    }

    #[test]
    fn an_unknown_destination_is_refused_with_what_is_on_offer() {
        let args = args_with(&["-I", "telepathy"]);
        let error = format!("{:#}", build_sink(&args).unwrap_err());
        assert!(error.contains("telepathy"), "{error}");
    }

    #[test]
    fn the_defaults_are_the_ones_documented() {
        let args = args_with(&[]);
        assert_eq!(args.paper_size, "iso_a4_210x297mm");
        assert_eq!(args.max_document_mb, 5);
        assert_eq!(args.max_raster_mb, 50);
        assert_eq!(args.max_queued_per_ip, 16);
        // No page limit unless one is asked for.
        assert!(args.max_pages.is_none());
        // Monochrome unless told otherwise.
        assert!(!args.color);
        assert!(media::find(&args.paper_size).is_some());
    }

    #[test]
    fn the_transport_limit_admits_the_larger_of_the_two() {
        let args = args_with(&[]);
        let limits = backend::Limits {
            document_bytes: args.max_document_mb * 1024 * 1024,
            raster_bytes: args.max_raster_mb * 1024 * 1024,
        };
        assert_eq!(limits.transport_bytes(), 50 * 1024 * 1024);
        assert_eq!(
            limits.for_format(print_document::Format::Pdf),
            5 * 1024 * 1024
        );
        assert_eq!(
            limits.for_format(print_document::Format::PwgRaster),
            50 * 1024 * 1024
        );
    }

    #[test]
    fn an_unknown_paper_size_is_refused_with_the_list_of_known_ones() {
        let args = args_with(&["--paper-size", "iso_a0_841x1189mm"]);
        assert!(media::find(&args.paper_size).is_none());
        // And a known one resolves.
        let args = args_with(&["--paper-size", "na_letter_8.5x11in"]);
        assert_eq!(media::find(&args.paper_size).unwrap().width, 21590);
    }

    #[test]
    fn verbose_is_off_unless_it_is_asked_for() {
        assert!(!args_with(&[]).verbose);
        assert!(args_with(&["--verbose"]).verbose);
        assert!(args_with(&["-v"]).verbose);
    }

    #[test]
    fn verbose_turns_the_debug_logs_on_for_the_printer_and_what_it_prints_with() {
        let quiet = default_logging(false);
        assert_eq!(
            quiet,
            "watermark_printer=info,ippserver=info,universal_print=info"
        );
        // Including the access log the IPP layer writes at debug.
        let verbose = default_logging(true);
        for crate_name in ["watermark_printer", "ippserver", "universal_print"] {
            assert!(
                verbose.contains(&format!("{crate_name}=debug")),
                "{verbose}"
            );
        }
        // Both are filters the subscriber will take.
        for directives in [quiet, verbose] {
            EnvFilter::builder().parse(&directives).expect(&directives);
        }
    }

    #[test]
    fn limits_are_read_from_the_command_line() {
        let args = args_with(&["--max-pages", "20", "--max-queued-per-ip", "3"]);
        assert_eq!(args.max_pages, Some(20));
        assert_eq!(args.max_queued_per_ip, 3);
    }

    #[test]
    fn the_printer_uuid_is_stable_for_a_name_and_differs_between_names() {
        let one = Uuid::new_v5(&UUID_NAMESPACE, b"Watermark Printer");
        let again = Uuid::new_v5(&UUID_NAMESPACE, b"Watermark Printer");
        let other = Uuid::new_v5(&UUID_NAMESPACE, b"Another Printer");
        assert_eq!(one, again);
        assert_ne!(one, other);
    }
}

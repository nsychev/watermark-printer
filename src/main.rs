use clap::Parser;
use ipp::prelude::Uri;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use uuid::Uuid;
use watermark_printer::handler::PrintJobHandler;
use watermark_printer::server::IppServer;
use watermark_printer::service::{PrinterInfoBuilder, SimpleIppService};

#[derive(Parser)]
struct Args {
    #[arg(short, long, help = "Printer name", default_value = "PDF-Printer")]
    name: String,
    #[arg(
        short = 'p',
        long,
        default_value = "631",
        help = "Address or port to bind (e.g. 0.0.0.0:631, [::1]:631 or 631)"
    )]
    bind: String,
    #[arg(
        short,
        long,
        default_value = "/tmp/printouts",
        help = "Path to store all PDFs"
    )]
    storage: String,
    #[arg(
        short,
        long,
        help = "Path to Lua script with custom `get_team_id' function"
    )]
    team_id_script: Option<String>,
    #[arg(short = 'I', long, help = "Next printer IPP URL")]
    next_ipp: Uri,
}

fn parse_bind_address(bind: &str) -> anyhow::Result<SocketAddr> {
    if let Ok(addr) = bind.parse::<SocketAddr>() {
        return Ok(addr);
    }

    if let Ok(port) = bind.parse::<u16>() {
        return Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port));
    }

    anyhow::bail!(
        "Invalid bind address: '{}'. Expected format: 'address:port' or just 'port'",
        bind
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let addr = parse_bind_address(&args.bind)?;

    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| addr.ip().to_string());
    let displayed_addr = format!("{}:{}", hostname, addr.port());

    let ipp_handler = PrintJobHandler::new(args.storage, args.team_id_script, args.next_ipp)?;
    let mut ipp_service = SimpleIppService::new(displayed_addr, ipp_handler);

    ipp_service.set_info(
        PrinterInfoBuilder::default()
            .name(args.name.clone())
            .uuid(Some(Uuid::new_v3(
                &Uuid::NAMESPACE_OID,
                args.name.as_bytes(),
            )))
            .build()
            .unwrap(),
    );

    println!("Listening on {}", addr);
    if let Err(e) = IppServer::serve(addr, Arc::new(ipp_service)).await {
        eprintln!("Server error: {}", e);
    }
    Ok(())
}

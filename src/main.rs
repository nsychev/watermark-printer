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
    #[arg(short, long, default_value = "631", help = "Port to listen on")]
    port: u16,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), args.port);

    let hostname = hostname::get()
        .unwrap_or_default()
        .into_string()
        .unwrap_or_default();
    let displayed_addr = format!("{}:{}", hostname, 631);

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

    println!("Listening on port {}", args.port);
    if let Err(e) = IppServer::serve(addr, Arc::new(ipp_service)).await {
        eprintln!("Server error: {}", e);
    }
    Ok(())
}

use ipp::prelude::Uri;
use nercprint::handler::PrintJobHandler;
use nercprint::server::IppServer;
use nercprint::service::{PrinterInfoBuilder, SimpleIppService};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use uuid::Uuid;
use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(short, long, help = "Printer name", default_value = "PDF-Printer")]
    name: String,
    #[arg(short, long, default_value = "631", help = "Port to listen on")]
    port: u16,
    #[arg(short, long, default_value = "/tmp/nercprint", help = "Path to store all PDFs")]
    storage: String,
    #[arg(short = 'I', long, help = "Next printer IPP URL")]
    next_ipp: Uri
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), args.port);

    let hostname = hostname::get().unwrap_or_default().into_string().unwrap_or_default();
    let displayed_addr = format!("{}:{}", hostname, 631);

    let mut ipp_handler: SimpleIppService<PrintJobHandler> = SimpleIppService::new(displayed_addr, PrintJobHandler::new(args.storage, args.next_ipp));

    ipp_handler.set_info(
        PrinterInfoBuilder::default()
            .name(args.name.clone())
            .uuid(Some(Uuid::new_v3(&Uuid::NAMESPACE_OID, args.name.as_bytes())))
            .build()
            .unwrap(),
    );

    println!("Listening on port {}", args.port);
    if let Err(e) = IppServer::serve(addr, Arc::new(ipp_handler)).await {
        eprintln!("Server error: {}", e);
    }
    Ok(())
}

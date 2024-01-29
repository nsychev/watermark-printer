use async_trait::async_trait;
use nercprint::server::IppServer;
use nercprint::service::{
    PrinterInfoBuilder, SimpleIppDocument, SimpleIppService, SimpleIppServiceHandler,
};
use nercprint::watermark::apply_watermark;
use futures::io::AsyncReadExt;
use printers::printer::Printer;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use uuid::Uuid;
use nercprint::drawer::WatermarkFactory;
use lopdf::Document;

struct MyHandler {
    watermark_factory: WatermarkFactory,
    printer: Printer
}

impl MyHandler {
    fn new(printer: Printer) -> Self {
        Self {
            watermark_factory: WatermarkFactory::new(),
            printer
        }
    }
}
#[async_trait]
impl SimpleIppServiceHandler for MyHandler {
    async fn handle_document(&self, mut ipp_document: SimpleIppDocument, remote_addr: &SocketAddr) -> anyhow::Result<()> {
        let team_id = match remote_addr {
            SocketAddr::V4(addr) => addr.ip().octets()[2],
            SocketAddr::V6(..) => {
                eprintln!("Job came from IPv6 client, skipped");
                return Ok(())
            }
        };

        let watermark = self.watermark_factory.draw(format!("{:0>3}", team_id), 595, 595);

        let mut content = Vec::new();
        ipp_document.payload.read_to_end(&mut content).await?;

        let mut file = File::create("source.bin").await?;
        file.write_all(&content).await?;

        let mut document = Document::load_mem(&content)?;
        eprintln!("Success!");

        apply_watermark(&mut document, watermark, 100.0, 100.0)?;

        document.save("1.pdf")?;
        self.printer.print_file("1.pdf", Some("1.pdf")).unwrap();

        Ok(())
    }
}

fn get_printer() -> anyhow::Result<Printer> {
    for printer in printers::get_printers() {
        println!("{}", printer.system_name);
        if printer.system_name == "Generic-PDF" {
            continue;
        }
        println!("Using printer: {}", printer.system_name);
        return Ok(printer);
    }
    return Err(anyhow::anyhow!("No printer found"));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 1337);
    let mut ipp_handler: SimpleIppService<MyHandler> = SimpleIppService::new(MyHandler::new(get_printer()?));
    ipp_handler.set_info(
        PrinterInfoBuilder::default()
            .name("NEF".to_string())
            .uuid(Some(
                Uuid::parse_str("786a551c-65a3-43ce-89ba-33c51bae9bc2").unwrap(),
            ))
            .build()
            .unwrap(),
    );
    if let Err(e) = IppServer::serve(addr, Arc::new(ipp_handler)).await {
        eprintln!("server error: {}", e);
    }
    Ok(())
}

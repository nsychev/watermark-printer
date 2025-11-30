use async_trait::async_trait;
use futures::AsyncReadExt;
use ipp::model::StatusCode;
use ipp::prelude::{AsyncIppClient, IppOperationBuilder, IppPayload, Uri};
use lopdf::Document;
use mlua::{Function, Lua};
use std::net::SocketAddr;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::drawer::WatermarkFactory;
use crate::error::IppError;
use crate::image_xobject::has_flipped_coordinates;
use crate::pjl::{PjlError, extract_content};
use crate::service::{SimpleIppDocument, SimpleIppServiceHandler};
use crate::watermark::apply_watermark;

use std::path::PathBuf;

static DEFAULT_TEAM_ID_SCRIPT: &str = include_str!("default_team_id_script.lua");

pub struct PrintJobHandler {
    storage: PathBuf,
    watermark_factory: WatermarkFactory,
    next_ipp_uri: Uri,
    next_ipp_client: AsyncIppClient,
    _lua: Lua, // extends lua lifetime
    get_team_id: Function,
}

impl PrintJobHandler {
    pub fn new(
        storage_path: String,
        team_id_script: Option<String>,
        next_ipp: Uri,
    ) -> anyhow::Result<Self> {
        let lua = Lua::new();

        lua.load(match team_id_script {
            Some(script) => std::fs::read_to_string(script).unwrap(),
            None => DEFAULT_TEAM_ID_SCRIPT.to_string(),
        })
        .exec()?;

        let get_team_id: Function = lua.globals().get("get_team_id")?;

        Ok(Self {
            storage: PathBuf::from(storage_path),
            watermark_factory: WatermarkFactory::new(),
            next_ipp_uri: next_ipp.clone(),
            next_ipp_client: AsyncIppClient::new(next_ipp),
            _lua: lua,
            get_team_id,
        })
    }
}

#[async_trait]
impl SimpleIppServiceHandler for PrintJobHandler {
    async fn handle_document(
        &self,
        mut ipp_document: SimpleIppDocument,
        remote_addr: &SocketAddr,
    ) -> anyhow::Result<()> {
        let team_id = match self
            .get_team_id
            .call::<Option<String>>(remote_addr.ip().to_string())?
        {
            Some(team_id) => team_id,
            None => {
                eprintln!(
                    "Unknown origin machine {}, skipping the job",
                    remote_addr.ip()
                );
                return Ok(());
            }
        };

        let addr_dir = self.storage.join(remote_addr.ip().to_string());
        tokio::fs::create_dir_all(&addr_dir).await?;
        let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let uuid = uuid::Uuid::new_v4();
        let filename = format!("{}-{}", timestamp, uuid);

        let mut content = Vec::new();
        ipp_document.payload.read_to_end(&mut content).await?;

        let raw_path = addr_dir.join(format!("{}.raw.pdf", filename));
        let mut raw_file = File::create(&raw_path).await?;
        raw_file.write_all(&content).await?;

        let content = match extract_content(&content) {
            Ok(pjl_content) => pjl_content,
            Err(PjlError::NotPjlFile) => content,
            Err(PjlError::UnsupportedPjlFile) => {
                eprintln!("Not a PJL file");
                return Err(IppError {
                    code: StatusCode::ClientErrorDocumentFormatNotSupported,
                    msg: StatusCode::ClientErrorDocumentFormatNotSupported.to_string(),
                }
                .into());
            }
        };

        let mut document = Document::load_mem(&content)?;

        let watermark =
            self.watermark_factory
                .draw(team_id, 595, 595, has_flipped_coordinates(&document));
        apply_watermark(&mut document, watermark, 0.0, 100.0)?;

        let pdf_path = addr_dir.join(format!("{}.pdf", filename));
        document.save(&pdf_path)?;

        let payload = IppPayload::new_async(File::open(&pdf_path).await?.compat());

        let operation = IppOperationBuilder::print_job(self.next_ipp_uri.clone(), payload)
            .job_title(filename)
            .build();

        if let Err(err) = self.next_ipp_client.send(operation).await {
            eprintln!("Failed to send job to next printer: {}", err);
            // TODO Error
        }

        std::fs::remove_file(raw_path)?;
        Ok(())
    }
}

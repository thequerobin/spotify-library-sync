use anyhow::{bail, Result};
use clap::Args;
use std::path::PathBuf;
use url::Url;

#[derive(Args, Clone, Debug)]
pub struct Config {
    #[arg(
        long,
        env = "SPOTIFY_DATABASE",
        default_value = "./data/spotify_library.db",
        global = true
    )]
    pub database: PathBuf,
    #[arg(long, env = "SPOTIFY_CLIENT_ID", global = true)]
    pub client_id: Option<String>,
    #[arg(
        long,
        env = "SPOTIFY_REDIRECT_URI",
        default_value = "http://127.0.0.1:8888/callback",
        global = true
    )]
    pub redirect_uri: String,
    #[arg(
        long,
        env = "SPOTIFY_REQUEST_INTERVAL_MS",
        default_value_t = 500,
        global = true
    )]
    pub request_interval_ms: u64,
    #[arg(
        long,
        env = "SPOTIFY_METADATA_TTL_DAYS",
        default_value_t = 30,
        global = true
    )]
    pub metadata_ttl_days: u32,
}

impl Config {
    pub fn client_id(&self) -> Result<&str> {
        self.client_id
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("Configure SPOTIFY_CLIENT_ID no .env ou use --client-id")
            })
    }
    pub fn redirect(&self) -> Result<Url> {
        let url = Url::parse(&self.redirect_uri)?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.port().is_none()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            bail!("Callback deve ser http://127.0.0.1:PORTA/caminho, sem query ou fragmento");
        }
        Ok(url)
    }
}

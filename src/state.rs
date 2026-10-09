use reqwest::Client;
use std::env;
use std::sync::Arc;
use tracing::{info, warn};

use crate::db::{connect_db, Db};

#[derive(Clone)]
pub struct AppState {
    pub db_client: Option<Arc<Db>>,
    pub http_client: Arc<Client>,
}

impl AppState {
    pub async fn new() -> Result<Self, String> {
        let http_client = Arc::new(create_client()?);

        let db_client = match env::var("DATABASE_URL") {
            Ok(database_url) => {
                let client = connect_db(&database_url).await?;
                info!("Connected to database");
                Some(Arc::new(client))
            }
            Err(_) => {
                warn!("DATABASE_URL not set. Database upload will be disabled.");
                None
            }
        };

        Ok(Self { db_client, http_client })
    }
}

fn create_client() -> Result<Client, String> {
    Client::builder()
        .cookie_store(true)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

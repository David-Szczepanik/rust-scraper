use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use postgrest::Postgrest;
use reqwest::Client;
use std::env;
use std::sync::Arc;
use tokio::task;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

mod models;
mod scrapers;

use models::*;
use scrapers::{scrape_nejvyssi, scrape_nejvyssi_spravni, scrape_ustavni};

async fn upload_to_supabase(
    client: &postgrest::Postgrest,
    results: &[CaseResult],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
   // filter duplicates
    let mut seen = std::collections::HashSet::new();
    let db_cases: Vec<DbCase> = results
        .iter()
        .filter(|c| seen.insert(c.spisova_znacka.clone()))
        .cloned()
        .map(DbCase::from)
        .collect();
    let body = serde_json::to_string(&db_cases)?;

    let resp = client
        .from("judikatura")
        .upsert(body)
        .on_conflict("jud_id")
        .execute()
        .await
        .map_err(|e| format!("Supabase request failed: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Supabase error {}: {}", status, text).into());
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rust_scraper=info".parse().unwrap()),
        )
        .init();

    let state = match AppState::new() {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to initialize app: {}", e);
            std::process::exit(1);
        }
    };

    // check for DEBUG mode
    let debug_mode: bool = env::var("DEBUG") == Ok("1".to_string());
    if debug_mode {
        info!("Running in DEBUG mode - executing hardcoded search cases");

        let payload = ScrapeRequest {
            task_id: "debug-task".to_string(),
            ustavni: Some(vec!["I.ÚS 2956/23".to_string()]),
            nejvyssi: Some(vec!["3 Tdo 706/2024".to_string()]),
            nejvyssi_spravni: Some(vec!["1 As 112/2024".to_string(), "10 As 222/2024".to_string(), "9 As 211/2022".to_string()]),
            debug_mode: Some(true),
        };


        let mut handles: Vec<task::JoinHandle<Result<CaseResult, (String, String)>>> = vec![];

        let mut tasks: Vec<(String, String)> = vec![];
        if let Some(cases) = &payload.ustavni {
            for c in cases { tasks.push((c.clone(), "ustavni".to_string())); }
        }
        if let Some(cases) = &payload.nejvyssi {
            for c in cases { tasks.push((c.clone(), "nejvyssi".to_string())); }
        }
        if let Some(cases) = &payload.nejvyssi_spravni {
            for c in cases { tasks.push((c.clone(), "nejvyssi_spravni".to_string())); }
        }

        for (case_number, court_type) in &tasks {
            let case = case_number.clone();
            let court = court_type.clone();
            let handle = task::spawn(async move {
                let client = create_client().expect("Failed to create client");
                let result = match court.as_str() {
                    "ustavni" => scrape_ustavni(&client, &case).await,
                    "nejvyssi" => scrape_nejvyssi(&client, &case).await,
                    "nejvyssi_spravni" => scrape_nejvyssi_spravni(&client, &case).await,
                    _ => scrape_ustavni(&client, &case).await,
                };
                match result {
                    Ok(res) => {
                        info!("Successfully scraped ({}): {}", court, res.spisova_znacka);
                        Ok(res)
                    }
                    Err(e) => {
                        error!("Failed to scrape {} ({}): {}", case, court, e);
                        Err((case, e.to_string()))
                    }
                }
            });
            handles.push(handle);
        }

        let mut results: Vec<CaseResult> = vec![];
        let mut failed_cases: std::collections::HashMap<String, String> = std::collections::HashMap::new();

        for handle in handles {
            match handle.await {
                Ok(Ok(result)) => results.push(result),
                Ok(Err((failed_case, error_msg))) => {
                    failed_cases.insert(failed_case, error_msg);
                }
                Err(e) => {
                    error!("Task panicked: {}", e);
                }
            }
        }

        // Retry failed cases once
        if !failed_cases.is_empty() {
            info!("Retrying {} failed cases...", failed_cases.len());
            let mut retry_handles: Vec<(String, task::JoinHandle<Result<CaseResult, (String, String)>>)> = vec![];

            for (case_number, court_type) in &tasks {
                if failed_cases.contains_key(case_number) {
                    let case = case_number.clone();
                    let court = court_type.clone();

                    let handle = task::spawn(async move {
                        let client = create_client().expect("Failed to create client");
                        let result = match court.as_str() {
                            "ustavni" => scrape_ustavni(&client, &case).await,
                            "nejvyssi" => scrape_nejvyssi(&client, &case).await,
                            "nejvyssi_spravni" => scrape_nejvyssi_spravni(&client, &case).await,
                            _ => scrape_ustavni(&client, &case).await,
                        };

                        match result {
                            Ok(res) => {
                                info!("Retry successful ({}): {}", court, res.spisova_znacka);
                                Ok(res)
                            }
                            Err(e) => {
                                error!("Retry failed {} ({}): {}", case, court, e);
                                Err((case, e.to_string()))
                            }
                        }
                    });
                    retry_handles.push((case_number.clone(), handle));
                }
            }

            for (original_case, handle) in retry_handles {
                match handle.await {
                    Ok(Ok(result)) => {
                        results.push(result);
                        failed_cases.remove(&original_case);
                    }
                    Ok(Err((failed_case, error_msg))) => {
                        failed_cases.insert(failed_case, error_msg);
                    }
                    Err(e) => {
                        error!("Retry task panicked: {}", e);
                    }
                }
            }
        }

        info!("DEBUG mode completed:");
        info!("  - Successful: {}", results.len());
        info!("  - Failed: {}", failed_cases.len());

        // upload to Supabase
        if !results.is_empty() {
            if let Some(client) = &state.supabase_client {
                info!("Uploading {} results to Supabase", results.len());
                if let Err(e) = upload_to_supabase(client, &results).await {
                    error!("Failed to upload to Supabase: {}", e);
                } else {
                    info!("Successfully uploaded results to Supabase");
                }
            }
        }

        let response: ScrapeResponse = ScrapeResponse {
            task_id: payload.task_id.clone(),
            scraped_count: results.len(),
            results: results.iter().cloned().map(ScrapedCase::from).collect(),
        };

        let log_data = ScrapeLog {
            success: failed_cases.is_empty(),
            task_id: payload.task_id,
            scraped_count: response.scraped_count,
            results: results, // Use full results here
            failed_cases,
            message: "DEBUG mode completed".to_string(),
        };

        if let Err(e) = save_log_file(&log_data) {
            error!("Failed to save log file: {}", e);
        }

        println!("{}", serde_json::to_string_pretty(&response).unwrap());

        return;
    }

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/scrape", post(scrape_handler))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // start server
    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{}", port);
    info!("Starting Rust scraper on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[derive(Clone)]
pub struct AppState {
    pub supabase_client: Option<Arc<Postgrest>>,
}

fn create_client() -> Result<Client, String> {
    Client::builder()
        .cookie_store(true)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

impl AppState {
    pub fn new() -> Result<Self, String> {
        let supabase_key = env::var("SUPABASE_SERVICE_ROLE_KEY")
            .or_else(|_| env::var("SUPABASE_ANON_KEY"))
            .or_else(|_| env::var("SUPABASE_KEY"));

        let supabase_client = if let (Ok(mut url), Ok(key)) = (env::var("SUPABASE_URL"), supabase_key) {
            if !url.ends_with("/rest/v1") && !url.ends_with("/rest/v1/") {
                url = format!("{}/rest/v1/", url.trim_end_matches('/'));
            }
            info!("Initializing Supabase client with URL: {}", url);
            Some(Arc::new(
                Postgrest::new(url)
                    .insert_header("apikey", &key)
                    .insert_header("Authorization", format!("Bearer {}", key)),
            ))
        } else {
            warn!("SUPABASE_URL or valid API KEY not set. Database upload will be disabled.");
            None
        };

        Ok(Self {
            supabase_client,
        })
    }
}

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn scrape_handler(
    State(state): State<AppState>,
    Json(payload): Json<ScrapeRequest>,
) -> (StatusCode, Json<ScrapeResponse>) {
    let is_debug: bool = payload.debug_mode.unwrap_or(false) || env::var("DEBUG") == Ok("1".to_string());

    info!(
        "Received scrape request for task_id: {}, debug_mode: {}",
        payload.task_id,
        is_debug
    );

    info!(
        "Received counts - Ustavni: {}, Nejvyssi: {}, Nejvyssi spravni: {}",
        payload.ustavni.as_ref().map(|v| v.len()).unwrap_or(0),
        payload.nejvyssi.as_ref().map(|v| v.len()).unwrap_or(0),
        payload.nejvyssi_spravni.as_ref().map(|v| v.len()).unwrap_or(0)
    );

    let mut tasks: Vec<(String, String)> = vec![];

    // Helper to add cases for a specific court
    let mut add_cases = |cases: &Option<Vec<String>>, court_type: &str| {
        if let Some(c) = cases {
            let mut list = c.clone();
            if is_debug && !list.is_empty() {
                list = vec![list[0].clone()];
            }
            for case in list {
                tasks.push((case, court_type.to_string()));
            }
        }
    };

    add_cases(&payload.ustavni, "ustavni");
    add_cases(&payload.nejvyssi, "nejvyssi");
    add_cases(&payload.nejvyssi_spravni, "nejvyssi_spravni");

    if tasks.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ScrapeResponse {
                task_id: payload.task_id,
                scraped_count: 0,
                results: vec![],
            }),
        );
    }

    let mut handles: Vec<task::JoinHandle<Result<CaseResult, (String, String)>>> = vec![];

    for (case_number, court_type) in &tasks {

        let case = case_number.clone();
        let court = court_type.clone();

        let handle = task::spawn(async move {
            let client = match create_client() {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to create client: {}", e);
                    return Err((case, e.to_string()));
                }
            };
            let result = match court.as_str() {
                "ustavni" => scrape_ustavni(&client, &case).await,
                "nejvyssi" => scrape_nejvyssi(&client, &case).await,
                "nejvyssi_spravni" => scrape_nejvyssi_spravni(&client, &case).await,
                _ => scrape_ustavni(&client, &case).await, // fallback
            };

            match result {
                Ok(res) => {
                    info!("Successfully scraped ({}): {}", court, res.spisova_znacka);
                    Ok(res)
                }
                Err(e) => {
                    error!("Failed to scrape {} ({}): {}", case, court, e);
                    Err((case, e.to_string()))
                }
            }
        });
        handles.push(handle);
    }

    let mut results: Vec<CaseResult> = vec![];
    let mut failed_cases: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for handle in handles {
        match handle.await {
            Ok(Ok(result)) => results.push(result),
            Ok(Err((failed_case, error_msg))) => {
                failed_cases.insert(failed_case, error_msg);
            }
            Err(e) => {
                error!("Task panicked: {}", e);
            }
        }
    }

    // retry failed cases once
    if !failed_cases.is_empty() {
        info!("Retrying {} failed cases...", failed_cases.len());
        let mut retry_handles: Vec<(String, task::JoinHandle<Result<CaseResult, (String, String)>>)> = vec![];

        for (case_number, court_type) in &tasks {
            if failed_cases.contains_key(case_number) {
                let case = case_number.clone();
                let court = court_type.clone();

                let handle = task::spawn(async move {
                    let client = match create_client() {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Failed to create client: {}", e);
                            return Err((case, e.to_string()));
                        }
                    };
                    let result = match court.as_str() {
                        "ustavni" => scrape_ustavni(&client, &case).await,
                        "nejvyssi" => scrape_nejvyssi(&client, &case).await,
                        "nejvyssi_spravni" => scrape_nejvyssi_spravni(&client, &case).await,
                        _ => scrape_ustavni(&client, &case).await, // fallback
                    };

                    match result {
                        Ok(res) => {
                            info!("Retry successful ({}): {}", court, res.spisova_znacka);
                            Ok(res)
                        }
                        Err(e) => {
                            error!("Retry failed {} ({}): {}", case, court, e);
                            Err((case, e.to_string()))
                        }
                    }
                });
                retry_handles.push((case_number.clone(), handle));
            }
        }

        for (original_case, handle) in retry_handles {
            match handle.await {
                Ok(Ok(result)) => {
                    results.push(result);
                    failed_cases.remove(&original_case);
                }
                Ok(Err((failed_case, error_msg))) => {
                    failed_cases.insert(failed_case, error_msg);
                }
                Err(e) => {
                    error!("Retry task panicked: {}", e);
                }
            }
        }
    }

    // upload to Supabase
    if !results.is_empty() {
        if let Some(client) = &state.supabase_client {
            info!("Uploading {} results to Supabase", results.len());
            if let Err(e) = upload_to_supabase(client, &results).await {
                error!("Failed to upload to Supabase: {}", e);
            } else {
                info!("Successfully uploaded results to Supabase");
            }
        }
    }

    let success = failed_cases.is_empty();
    let processed_count = results.len();
    let status = if success {
        StatusCode::OK
    } else if processed_count > 0 {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };

    let failed_count = failed_cases.len();
    let response = ScrapeResponse {
        task_id: payload.task_id.clone(),
        scraped_count: processed_count,
        results: results.iter().cloned().map(ScrapedCase::from).collect(),
    };

    let log_data = ScrapeLog {
        success,
        task_id: payload.task_id,
        scraped_count: response.scraped_count,
        results: results, // Use full results here
        failed_cases,
        message: format!(
            "Processed {} cases, {} failed",
            processed_count,
            failed_count
        ),
    };

    if let Err(e) = save_log_file(&log_data) {
        error!("Failed to save log file: {}", e);
    }

    (status, Json(response))
}

fn save_log_file(response: &ScrapeLog) -> std::io::Result<()> {
    use chrono::Local;
    use std::fs;
    use std::path::Path;

    let dir_path = Path::new("_LOGS/scraper");
    if !dir_path.exists() {
        fs::create_dir_all(dir_path)?;
    }

    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let filename = format!("{}.json", timestamp);
    let file_path = dir_path.join(filename);

    let json = serde_json::to_string_pretty(response)?;
    fs::write(&file_path, json)?;

    info!("Saved scrape log to {:?}", file_path);
    Ok(())
}

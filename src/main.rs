use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use reqwest::Client;
use std::env;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tokio::task;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

mod models;
mod scrapers;

use models::*;
use scrapers::{scrape_nejvyssi, scrape_nejvyssi_spravni, scrape_ustavni};

async fn upsert_to_db(
    db: &tokio_postgres::Client,
    results: &[CaseResult],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<&CaseResult> = results
        .iter()
        .filter(|c| seen.insert(c.spisova_znacka.replace(' ', "")))
        .collect();

    for case in unique {
        let datum: Option<chrono::NaiveDate> = if case.datum_rozhodnuti.is_empty() {
            None
        } else {
            chrono::NaiveDate::parse_from_str(&case.datum_rozhodnuti, "%d.%m.%Y")
                .or_else(|_| chrono::NaiveDate::parse_from_str(&case.datum_rozhodnuti, "%Y-%m-%d"))
                .ok()
        };
        let soud = case.soud.first().map(|s| s.as_str()).unwrap_or("unknown");

        db.execute(
            "INSERT INTO judikatura (
                spisova_znacka, datum_rozhodnuti, soud, popularni_nazev, pravni_veta,
                text_dokumentu, abstrakt, ecli, kategorie
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (spisova_znacka) DO UPDATE SET
                datum_rozhodnuti = COALESCE(EXCLUDED.datum_rozhodnuti, judikatura.datum_rozhodnuti),
                soud = COALESCE(EXCLUDED.soud, judikatura.soud),
                popularni_nazev = COALESCE(EXCLUDED.popularni_nazev, judikatura.popularni_nazev),
                pravni_veta = COALESCE(EXCLUDED.pravni_veta, judikatura.pravni_veta),
                text_dokumentu = COALESCE(EXCLUDED.text_dokumentu, judikatura.text_dokumentu),
                abstrakt = COALESCE(EXCLUDED.abstrakt, judikatura.abstrakt),
                ecli = COALESCE(EXCLUDED.ecli, judikatura.ecli),
                kategorie = COALESCE(EXCLUDED.kategorie, judikatura.kategorie),
                updated_at = CURRENT_TIMESTAMP(0)",
            &[
                &case.spisova_znacka,
                &datum,
                &soud,
                &case.popularni_nazev.as_deref(),
                &case.pravni_veta.as_str(),
                &case.text_dokumentu.as_str(),
                &case.abstrakt.as_deref(),
                &case.ecli.as_str(),
                &case.kategorie.as_deref(),
            ],
        ).await.map_err(|e| {
            error!("Database execute error for {}: {}", case.spisova_znacka, e);
            e
        })?;
    }

    info!("Upserted {} cases to database", seen.len());
    Ok(())
}


// scraper/scrape
async fn get_cases_from_db_bulk(
    db: &tokio_postgres::Client,
    spisova_znacky: &[String],
) -> Result<std::collections::HashMap<String, (String, String)>, Box<dyn std::error::Error + Send + Sync>> {
    let rows = db.query(
        "SELECT j.spisova_znacka, j.soud, s AS original_query
         FROM judikatura j
         JOIN unnest($1::text[]) AS s 
           ON j.spisova_znacka_norm 
              LIKE LOWER(REPLACE(REPLACE(immutable_unaccent(SPLIT_PART(s, ' - ', 1)), ' ', ''), '.', '')) || '%'",
        &[&spisova_znacky],
    ).await?;

    let mut results = std::collections::HashMap::new();
    for row in rows {
        let db_spec_zn: String = row.get(0);
        let soud: Option<String> = row.get(1);
        let original_query: String = row.get(2);
        results.insert(original_query, (db_spec_zn, soud.unwrap_or_else(|| "unknown".to_string())));
    }
    Ok(results)
}

async fn get_case_from_db(
    db: &tokio_postgres::Client,
    spisova_znacka: &str,
) -> Result<Option<CaseResult>, Box<dyn std::error::Error + Send + Sync>> {
    let mut results = get_cases_from_db_bulk(db, &[spisova_znacka.to_string()]).await?;
    Ok(results.remove(spisova_znacka).map(|(id, soud)| CaseResult {
        spisova_znacka: id,
        soud: vec![soud],
        found_in_db: true,
        ..Default::default()
    }))
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

    let state = match AppState::new().await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to initialize app: {}", e);
            std::process::exit(1);
        }
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let scraper_routes = Router::new()
        .route("/scrape", post(scrape_handler))
        .route("/search", post(search_handler));

    let app = Router::new()
        .route("/health", get(health_handler))
        .nest("/scraper", scraper_routes)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // start server
    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{}", port);
    info!("Starting Rust Scraper on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[derive(Clone)]
pub struct AppState {
    pub db_client: Option<Arc<tokio_postgres::Client>>,
    pub http_client: Arc<Client>,
}

fn create_client() -> Result<Client, String> {
    Client::builder()
        .cookie_store(true)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

impl AppState {
    pub async fn new() -> Result<Self, String> {
        let http_client = Arc::new(create_client()?);

        let db_client = if let Ok(database_url) = env::var("DATABASE_URL") {
            info!("Connecting to database...");
            let (client, connection) = tokio_postgres::connect(&database_url, NoTls)
                .await
                .map_err(|e| format!("Failed to connect to database: {}", e))?;

            // Spawn connection handler in background
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    error!("Database connection error: {}", e);
                }
            });

            info!("Connected to database");
            Some(Arc::new(client))
        } else {
            warn!("DATABASE_URL not set. Database upload will be disabled.");
            None
        };

        Ok(Self { db_client, http_client })
    }
}

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

// { "task_id": "123", "courts": ["ustavni", "nejvyssi"], "phrases": ["phrase1", "phrase2"], "keywords": ["keyword1", "keyword2"] }
async fn scrape_handler(
    State(state): State<AppState>,
    Json(payload): Json<ScrapeRequest>,
) -> (StatusCode, Json<ScrapeResponse>) {
    let is_debug: bool = payload.debug_mode.unwrap_or(false) || env::var("DEBUG") == Ok("1".to_string());
    let scrape_limit = payload.limit.unwrap_or(5);

    info!(
        "Starting Rust Scraper: task_id: {}, debug_mode: {}, limit: {}, cases: (ustavni: {:?}, nejvyssi: {:?}, nejvyssi_spravni: {:?})",
        payload.task_id,
        is_debug,
        scrape_limit,
        payload.ustavni,
        payload.nejvyssi,
        payload.nejvyssi_spravni
    );

    let mut tasks: Vec<(String, String)> = vec![];

    // Helper to add cases for a specific court
    let mut add_cases = |cases: &Option<Vec<String>>, court_type: &str| {
        if let Some(c) = cases {
            let mut list = c.clone();
            if is_debug && !list.is_empty() {
                list = vec![list[0].clone()];
            }
            // Limit the number of cases per court type
            for case in list.into_iter().take(scrape_limit) {
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
                results: std::collections::HashMap::new(),
            }),
        );
    }

    let mut results: Vec<CaseResult> = vec![];
    let mut web_scraped_count = 0;
    let mut remaining_tasks: Vec<(String, String)> = vec![];

    // 1. Check DB first for all requested cases in bulk
    if let Some(db) = &state.db_client {
        let case_numbers: Vec<String> = tasks.iter().map(|(n, _)| n.clone()).collect();
        match get_cases_from_db_bulk(db, &case_numbers).await {
            Ok(found_map) => {
                info!("Bulk DB check found {}/{} cases", found_map.len(), tasks.len());
                for (case_number, court_type) in tasks {
                    if let Some((db_spec_zn, db_soud)) = found_map.get(&case_number) {
                        info!("Case {} found in database as {}, skipping scrape", case_number, db_spec_zn);
                        results.push(CaseResult {
                            spisova_znacka: db_spec_zn.clone(),
                            soud: vec![db_soud.clone()],
                            found_in_db: true,
                            ..Default::default()
                        });
                    } else {
                        remaining_tasks.push((case_number, court_type));
                    }
                }
            }
            Err(e) => {
                error!("Bulk database check failed: {}, attempting manual scrape for all", e);
                remaining_tasks = tasks;
            }
        }
    } else {
        remaining_tasks = tasks;
    }

    if remaining_tasks.is_empty() && !results.is_empty() {
        info!("All {} cases found in database.", results.len());
    } else if !remaining_tasks.is_empty() {
        info!("Need to scrape {} cases: {:?}.", remaining_tasks.len(), remaining_tasks.iter().map(|(n, _)| n).collect::<Vec<_>>());
    }

    let mut handles: Vec<task::JoinHandle<Result<CaseResult, (String, String)>>> = vec![];

    for (case_number, court_type) in remaining_tasks {
        let case = case_number.clone();
        let court = court_type.clone();
        let db_client = state.db_client.clone();
        let client = state.http_client.clone();

        let handle = task::spawn(async move {
            let result = match court.as_str() {
                "ustavni" => scrape_ustavni(&client, &case).await,
                "nejvyssi" => scrape_nejvyssi(&client, &case).await,
                "nejvyssi_spravni" => scrape_nejvyssi_spravni(&client, &case).await,
                _ => scrape_ustavni(&client, &case).await, // fallback
            };

            match result {
                Ok(res) => {
                    info!("Successfully scraped ({}): {}", court, res.spisova_znacka);
                    // Save to DB
                    if let Some(db) = &db_client {
                        let _ = upsert_to_db(db, &[res.clone()]).await;
                    }
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

    let mut failed_cases: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for handle in handles {
        match handle.await {
            Ok(Ok(result)) => {
                results.push(result);
                web_scraped_count += 1;
            }
            Ok(Err((failed_case, error_msg))) => {
                failed_cases.insert(failed_case, error_msg);
            }
            Err(e) => {
                error!("Task panicked: {}", e);
            }
        }
    }

    let success = failed_cases.is_empty();
    let processed_count = results.len();
    
    let mut grouped_results = std::collections::HashMap::new();
    for res in &results {
        let court_name = res.soud.first().map(|s| s.to_lowercase()).unwrap_or_else(|| "unknown".to_string());
        let key = if court_name.contains("ústavní") || court_name.contains("ustavni") {
            "ustavni"
        } else if court_name.contains("nejvyšší správní") || court_name.contains("spravni") {
            "nejvyssi_spravni"
        } else if court_name.contains("nejvyšší") || court_name.contains("nejvyssi") {
            "nejvyssi"
        } else {
            "other"
        };
        grouped_results.entry(key.to_string()).or_insert_with(Vec::new).push(res.spisova_znacka.clone());
    }

    let response = ScrapeResponse {
        task_id: payload.task_id.clone(),
        scraped_count: web_scraped_count,
        results: grouped_results,
    };

    let failed_count = failed_cases.len();
    let log_data = ScrapeLog {
        success,
        task_id: payload.task_id,
        scraped_count: response.scraped_count,
        results: results.iter().cloned().map(|r| {
            if r.found_in_db {
                LogResult::InDatabase {
                    spisova_znacka: r.spisova_znacka,
                    found_in_db: true,
                }
            } else {
                LogResult::Scraped(r)
            }
        }).collect(),
        failed_cases,
        message: format!("Processed {} cases, {} failed", processed_count, failed_count),
    };

    if let Err(e) = save_log_file(&log_data) {
        error!("Failed to save log file: {}", e);
    }

    let status = if success {
        StatusCode::OK
    } else if processed_count > 0 {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };

    (status, Json(response))
}

// POST /scraper/search
async fn search_handler(
    State(state): State<AppState>,
    Json(payload): Json<SearchRequest>,
) -> (StatusCode, Json<ScrapeResponse>) {
    let phrases: Vec<String> = payload.phrases.as_ref().map(|v| v.iter())
        .unwrap_or_default()
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .collect();
    let keywords: Vec<String> = payload.keywords.as_ref().map(|v| v.iter())
        .unwrap_or_default()
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .collect();
    
    let search_limit = payload.limit.unwrap_or(5);

    info!(
        "Search request: task_id={}, courts={:?}, phrases={}, keywords={}, limit={}",
        payload.task_id,
        payload.courts,
        phrases.len(),
        keywords.len(),
        search_limit
    );

    let filtered_courts = payload.courts.unwrap_or_default();

    if (phrases.is_empty() && keywords.is_empty()) || filtered_courts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ScrapeResponse {
                task_id: payload.task_id,
                scraped_count: 0,
                results: std::collections::HashMap::new(),
            }),
        );
    }

    let mut handles = vec![];

    for court in &filtered_courts {
        let court = court.clone();
        let phrases = phrases.clone();
        let keywords = keywords.clone();
        let db_client = state.db_client.clone();
        let client = state.http_client.clone();
        let current_limit = search_limit;

        let handle = task::spawn(async move {
            match court.as_str() {
                "ustavni" => {
                    let (k_citations, p_citations) = scrapers::search_ustavni_citations(&client, &phrases, &keywords)
                        .await
                        .map_err(|e| (court.clone(), e.to_string()))?;

                    let mut k_cit_map = std::collections::HashMap::new();
                    let mut p_cit_map = std::collections::HashMap::new();
                    let mut unique_cits = std::collections::HashMap::new();

                    for (term, hrefs) in k_citations {
                        let mut cits = Vec::new();
                        for (href, citation) in hrefs.into_iter().take(current_limit) {
                            let norm = citation.replace(' ', "");
                            cits.push(norm.clone());
                            unique_cits.insert(norm, (href, citation));
                        }
                        k_cit_map.insert(term, cits);
                    }
                    for (term, hrefs) in p_citations {
                        let mut cits = Vec::new();
                        for (href, citation) in hrefs.into_iter().take(current_limit) {
                            let norm = citation.replace(' ', "");
                            cits.push(norm.clone());
                            unique_cits.insert(norm, (href, citation));
                        }
                        p_cit_map.insert(term, cits);
                    }

                    let mut final_cases = std::collections::HashMap::new();
                    let mut fetch_set = task::JoinSet::new();

                    for (norm, (href, citation)) in unique_cits {
                        let db = db_client.clone();
                        let client = client.clone();
                        fetch_set.spawn(async move {
                            if let Some(db) = &db {
                                if let Ok(Some(existing)) = get_case_from_db(db, &citation).await {
                                    info!("Case {} found in database, skipping scrape", citation);
                                    return Ok((norm, existing));
                                }
                            }
                            info!("Case {} not in database, fetching details...", citation);
                            match scrapers::fetch_case_detail(&client, &href, &citation).await {
                                Ok(new_case) => {
                                    if let Some(db) = &db {
                                        let _ = upsert_to_db(db, &[new_case.clone()]).await;
                                    }
                                    Ok((norm, new_case))
                                }
                                Err(e) => Err(e.to_string()),
                            }
                        });
                    }

                    while let Some(res) = fetch_set.join_next().await {
                        match res {
                            Ok(Ok((norm, case))) => { final_cases.insert(norm, case); }
                            Ok(Err(e)) => warn!("Failed to fetch case: {}", e),
                            Err(e) => error!("Fetch task panicked: {}", e),
                        }
                    }

                    let mut k_results = std::collections::HashMap::new();
                    let mut p_results = std::collections::HashMap::new();
                    for (term, cits) in k_cit_map {
                        let results: Vec<CaseResult> = cits.into_iter()
                            .filter_map(|n| final_cases.get(&n).cloned())
                            .collect();
                        k_results.insert(term, results);
                    }
                    for (term, cits) in p_cit_map {
                        let results: Vec<CaseResult> = cits.into_iter()
                            .filter_map(|n| final_cases.get(&n).cloned())
                            .collect();
                        p_results.insert(term, results);
                    }
                    
                    Ok((court, k_results, p_results))
                }
                "nejvyssi" | "nejvyssi_spravni" => {
                    let kw = keywords.clone();
                    let ph = phrases.clone();
                    let mut fetch_set = task::JoinSet::new();
                    let terms: Vec<String> = kw.into_iter().take(current_limit)
                        .chain(ph.into_iter().take(current_limit))
                        .collect();
                    let mut final_cases = std::collections::HashMap::new();

                    for term in terms {
                        let db = db_client.clone();
                        let client = client.clone();
                        let court_str = court.clone();
                        let term_to_spawn = term.clone();
                        fetch_set.spawn(async move {
                            if let Some(db) = &db {
                                if let Ok(Some(existing)) = get_case_from_db(db, &term_to_spawn).await {
                                    info!("Case {} found in database, skipping scrape", term_to_spawn);
                                    return Ok((term_to_spawn, existing));
                                }
                            }
                            info!("Case {} not in database, scraping...", term_to_spawn);
                            let res = match court_str.as_str() {
                                "nejvyssi" => scrape_nejvyssi(&client, &term_to_spawn).await,
                                _ => scrape_nejvyssi_spravni(&client, &term_to_spawn).await,
                            };
                            match res {
                                Ok(new_case) => {
                                    if let Some(db) = &db {
                                        let _ = upsert_to_db(db, &[new_case.clone()]).await;
                                    }
                                    Ok((term_to_spawn, new_case))
                                }
                                Err(e) => Err(e.to_string()),
                            }
                        });
                    }

                    while let Some(res) = fetch_set.join_next().await {
                        match res {
                            Ok(Ok((term, case))) => { final_cases.insert(term, case); }
                            Ok(Err(e)) => warn!("Failed to fetch Supreme case: {}", e),
                            Err(e) => error!("Supreme fetch task panicked: {}", e),
                        }
                    }

                    let mut k_results = std::collections::HashMap::new();
                    let mut p_results = std::collections::HashMap::new();
                    for k in keywords {
                        let res = final_cases.get(&k).cloned().map(|c| vec![c]).unwrap_or_default();
                        k_results.insert(k, res);
                    }
                    for p in phrases {
                        let res = final_cases.get(&p).cloned().map(|c| vec![c]).unwrap_or_default();
                        p_results.insert(p, res);
                    }
                    
                    Ok((court, k_results, p_results))
                }
                _ => {
                    warn!("Unknown court type '{}' in search", court);
                    Err((court.clone(), format!("Unknown court type: {}", court)))
                }
            }
        });
        handles.push(handle);
    }

    let mut ustavni_res = None;
    let mut nejvyssi_res = None;
    let mut nejvyssi_spravni_res = None;
    let mut combined_results = vec![];
    let mut failed_cases = std::collections::HashMap::new();

    for handle in handles {
        match handle.await {
            Ok(Ok((court, k_map, p_map))) => {
                let mut cr = CourtResults::default();
                let mut current_court_cases = vec![];
                for (term, cases) in k_map {
                    cr.keywords.insert(term, cases.iter().cloned().map(ScrapedCase::from).collect());
                    current_court_cases.extend(cases);
                }
                for (term, cases) in p_map {
                    cr.phrases.insert(term, cases.iter().cloned().map(ScrapedCase::from).collect());
                    current_court_cases.extend(cases);
                }
                
                match court.as_str() {
                    "ustavni" => ustavni_res = Some(cr),
                    "nejvyssi" => nejvyssi_res = Some(cr),
                    "nejvyssi_spravni" => nejvyssi_spravni_res = Some(cr),
                    _ => {}
                }
                combined_results.extend(current_court_cases);
            }
            Ok(Err((ctx, err))) => { failed_cases.insert(ctx, err); }
            Err(e) => error!("Search task panicked: {}", e),
        }
    }

    let success = failed_cases.is_empty();
    let processed_count = combined_results.len();
    let status = if success {
        StatusCode::OK
    } else if processed_count > 0 {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };

    let log_data = ScrapeLog {
        success,
        task_id: payload.task_id.clone(),
        scraped_count: processed_count,
        results: combined_results.iter().cloned().map(|r| {
            if r.found_in_db {
                LogResult::InDatabase {
                    spisova_znacka: r.spisova_znacka,
                    found_in_db: true,
                }
            } else {
                LogResult::Scraped(r)
            }
        }).collect(),
        failed_cases,
        message: format!("Searched {} courts, {} total results", filtered_courts.len(), processed_count),
    };

    if let Err(e) = save_log_file(&log_data) {
        error!("Failed to save search log file: {}", e);
    }

    let mut grouped_results = std::collections::HashMap::new();
    if let Some(res) = ustavni_res {
        let mut ids = Vec::new();
        for list in res.keywords.values() { for c in list { ids.push(c.spisova_znacka.clone()); } }
        for list in res.phrases.values() { for c in list { ids.push(c.spisova_znacka.clone()); } }
        grouped_results.insert("ustavni".to_string(), ids);
    }
    if let Some(res) = nejvyssi_res {
        let mut ids = Vec::new();
        for list in res.keywords.values() { for c in list { ids.push(c.spisova_znacka.clone()); } }
        for list in res.phrases.values() { for c in list { ids.push(c.spisova_znacka.clone()); } }
        grouped_results.insert("nejvyssi".to_string(), ids);
    }
    if let Some(res) = nejvyssi_spravni_res {
        let mut ids = Vec::new();
        for list in res.keywords.values() { for c in list { ids.push(c.spisova_znacka.clone()); } }
        for list in res.phrases.values() { for c in list { ids.push(c.spisova_znacka.clone()); } }
        grouped_results.insert("nejvyssi_spravni".to_string(), ids);
    }

    let response = ScrapeResponse {
        task_id: payload.task_id,
        scraped_count: processed_count,
        results: grouped_results,
    };

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

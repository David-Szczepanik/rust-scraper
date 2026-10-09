use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use axum_extra::extract::Query;
use reqwest::Client;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::task;
use tracing::{error, info, warn};

use crate::db::{find_or_scrape, get_cases_from_db_bulk, save_to_db, Db};
use crate::models::*;
use crate::scrapers::{self, scrape_nejvyssi, scrape_nejvyssi_spravni, scrape_ustavni, BoxError};
use crate::state::AppState;

/// Liveness check
#[utoipa::path(
    get,
    path = "/health",
    tag = "health",
    responses((status = 200, description = "The service is up", body = HealthResponse))
)]
pub async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// Fetch cases by case number
///
/// Scrapes up to `limit` cases per court (only the first one in debug mode) and stores them.
/// Cases already in the database are not scraped again. Database lookup ignores accents,
/// spaces, dots and letter case, and accepts a prefix match, so `Pl. ÚS 19/93` matches
/// a stored `Pl.US19/93#1`.
#[utoipa::path(
    post,
    path = "/cases",
    tag = "cases",
    request_body = ScrapeRequest,
    responses(
        (status = 200, description = "Every case succeeded", body = CasesResponse),
        (status = 207, description = "Some cases failed, see `failed`", body = CasesResponse),
        (status = 422, description = "No case numbers given", body = ErrorResponse),
        (status = 502, description = "Every case failed", body = CasesResponse),
    )
)]
pub async fn scrape_handler(
    State(state): State<AppState>,
    Json(payload): Json<ScrapeRequest>,
) -> Response {
    let start_time = std::time::Instant::now();
    let payload_json = serde_json::to_string_pretty(&payload).unwrap_or_default();
    info!("Received scrape request:\n{}", payload_json);

    let is_debug = payload.debug_mode.unwrap_or(false) || env::var("DEBUG").as_deref() == Ok("1");
    let scrape_limit = payload.limit.unwrap_or(5);

    info!(
        "Starting Rust Scraper: task_id: {:?}, debug_mode: {}, limit: {}, cases: (ustavni: {:?}, nejvyssi: {:?}, nejvyssi_spravni: {:?})",
        payload.task_id,
        is_debug,
        scrape_limit,
        payload.ustavni,
        payload.nejvyssi,
        payload.nejvyssi_spravni
    );

    // (case number, court key)
    let per_court = if is_debug { scrape_limit.min(1) } else { scrape_limit };
    let tasks: Vec<(String, String)> = [
        (&payload.ustavni, "ustavni"),
        (&payload.nejvyssi, "nejvyssi"),
        (&payload.nejvyssi_spravni, "nejvyssi_spravni"),
    ]
    .into_iter()
    .flat_map(|(cases, court)| {
        cases.iter().flatten().take(per_court).map(move |case| (case.clone(), court.to_string()))
    })
    .collect();

    if tasks.is_empty() {
        return unprocessable("no case numbers given");
    }

    let mut results: Vec<CaseResult> = vec![];

    // Check the database for all cases in one query; scrape only the rest
    let remaining_tasks = match &state.db_client {
        Some(db) => {
            let case_numbers: Vec<String> = tasks.iter().map(|(n, _)| n.clone()).collect();
            match get_cases_from_db_bulk(db, &case_numbers).await {
                Ok(found_map) => {
                    info!("Bulk DB check found {}/{} cases", found_map.len(), tasks.len());
                    let mut remaining = vec![];
                    for (case_number, court_type) in tasks {
                        match found_map.get(&case_number) {
                            Some((db_spec_zn, db_soud)) => {
                                info!("Case {} found in database as {}, skipping scrape", case_number, db_spec_zn);
                                results.push(CaseResult::from_db(db_spec_zn.clone(), db_soud.clone()));
                            }
                            None => remaining.push((case_number, court_type)),
                        }
                    }
                    remaining
                }
                Err(e) => {
                    error!("Bulk database check failed: {}, attempting manual scrape for all", e);
                    tasks
                }
            }
        }
        None => tasks,
    };

    if remaining_tasks.is_empty() && !results.is_empty() {
        info!("All {} cases found in database.", results.len());
    } else if !remaining_tasks.is_empty() {
        info!("Need to scrape {} cases: {:?}.", remaining_tasks.len(), remaining_tasks.iter().map(|(n, _)| n).collect::<Vec<_>>());
    }

    let handles: Vec<_> = remaining_tasks
        .into_iter()
        .map(|(case, court)| {
            let db_client = state.db_client.clone();
            let client = state.http_client.clone();
            task::spawn(async move {
                match scrape_case(&client, &court, &case).await {
                    Ok(res) => {
                        info!("Successfully scraped ({}): {}", court, res.spisova_znacka);
                        save_to_db(db_client.as_deref(), &res).await;
                        Ok(res)
                    }
                    Err(e) => {
                        error!("Failed to scrape {} ({}): {}", case, court, e);
                        Err((case, e.to_string()))
                    }
                }
            })
        })
        .collect();

    let mut web_scraped_count = 0;
    let mut failed_cases = HashMap::new();

    for handle in handles {
        match handle.await {
            Ok(Ok(result)) => {
                results.push(result);
                web_scraped_count += 1;
            }
            Ok(Err((failed_case, error_msg))) => {
                failed_cases.insert(failed_case, error_msg);
            }
            Err(e) => error!("Task panicked: {}", e),
        }
    }

    let success = failed_cases.is_empty();
    let processed_count = results.len();

    let mut grouped_results: HashMap<String, Vec<String>> = HashMap::new();
    for res in &results {
        let key = res.soud.first().map_or("other", |s| court_key(s));
        grouped_results.entry(key.to_string()).or_default().push(res.spisova_znacka.clone());
    }

    let failed_count = failed_cases.len();
    let log_data = ScrapeLog {
        success,
        task_id: payload.task_id.clone().unwrap_or_default(),
        scraped_count: web_scraped_count,
        results: results.into_iter().map(LogResult::from).collect(),
        failed_cases: failed_cases.clone(),
        message: format!("Processed {} cases, {} failed", processed_count, failed_count),
    };

    if let Err(e) = save_log_file(&log_data) {
        error!("Failed to save log file: {}", e);
    }

    let response = CasesResponse {
        task_id: payload.task_id,
        scraped_count: web_scraped_count,
        results: grouped_results,
        failed: failed_cases,
    };

    info!("Scrape handler completed in {:?}", start_time.elapsed());
    (response_status(success, processed_count), Json(response)).into_response()
}

/// Search cases by phrase or keyword
///
/// Only the Constitutional Court (`ustavni`) has a real full-text search, returning up to
/// `limit` hits per term. For `nejvyssi` and `nejvyssi_spravni`, each term is treated as a
/// case number and scraped directly. Cases found are stored like in `POST /cases`.
#[utoipa::path(
    get,
    path = "/cases/search",
    tag = "cases",
    params(SearchRequest),
    responses(
        (status = 200, description = "Every court succeeded", body = CasesResponse),
        (status = 207, description = "Some courts failed, see `failed`", body = CasesResponse),
        (status = 422, description = "No court, no search term, or an unknown court", body = ErrorResponse),
        (status = 502, description = "Every court failed", body = CasesResponse),
    )
)]
pub async fn search_handler(
    State(state): State<AppState>,
    Query(payload): Query<SearchRequest>,
) -> Response {
    let start_time = std::time::Instant::now();
    let payload_json = serde_json::to_string_pretty(&payload).unwrap_or_default();
    info!("Received search request:\n{}", payload_json);

    let non_blank = |terms: &[String]| -> Vec<String> {
        terms.iter().filter(|s| !s.trim().is_empty()).cloned().collect()
    };
    let phrases = non_blank(&payload.phrases);
    let keywords = non_blank(&payload.keywords);
    let search_limit = payload.limit.unwrap_or(5);

    info!(
        "Search request: task_id={:?}, courts={:?}, phrases={}, keywords={}, limit={}",
        payload.task_id,
        payload.courts,
        phrases.len(),
        keywords.len(),
        search_limit
    );

    let courts = payload.courts;

    if courts.is_empty() {
        return unprocessable("no court given");
    }
    if phrases.is_empty() && keywords.is_empty() {
        return unprocessable("no phrase or keyword given");
    }
    if let Some(unknown) = courts.iter().find(|c| !COURTS.contains(&c.as_str())) {
        return unprocessable(format!("unknown court: {}", unknown));
    }

    let handles: Vec<_> = courts
        .iter()
        .map(|court| {
            task::spawn(search_court(
                court.clone(),
                phrases.clone(),
                keywords.clone(),
                search_limit,
                state.db_client.clone(),
                state.http_client.clone(),
            ))
        })
        .collect();

    let mut grouped_results = HashMap::new();
    let mut combined_results = vec![];
    let mut failed_cases = HashMap::new();

    for handle in handles {
        match handle.await {
            Ok(Ok((court, cases))) => {
                let ids = cases.iter().map(|c| c.spisova_znacka.clone()).collect::<Vec<_>>();
                grouped_results.insert(court, ids);
                combined_results.extend(cases);
            }
            Ok(Err((court, err))) => { failed_cases.insert(court, err); }
            Err(e) => error!("Search task panicked: {}", e),
        }
    }

    let success = failed_cases.is_empty();
    let processed_count = combined_results.len();

    let log_data = ScrapeLog {
        success,
        task_id: payload.task_id.clone().unwrap_or_default(),
        scraped_count: processed_count,
        results: combined_results.into_iter().map(LogResult::from).collect(),
        failed_cases: failed_cases.clone(),
        message: format!("Searched {} courts, {} total results", courts.len(), processed_count),
    };

    if let Err(e) = save_log_file(&log_data) {
        error!("Failed to save search log file: {}", e);
    }

    let response = CasesResponse {
        task_id: payload.task_id,
        scraped_count: processed_count,
        results: grouped_results,
        failed: failed_cases,
    };

    info!("Search handler completed in {:?}", start_time.elapsed());
    (response_status(success, processed_count), Json(response)).into_response()
}

/// Runs a search in one court and returns the cases found.
///
/// The Constitutional Court (ustavni) has full-text search, so each term gives up to `limit` hits.
/// The Supreme Courts have no search here: each term is treated as a case number, and only
/// the first `limit` keywords and `limit` phrases are scraped.
async fn search_court(
    court: String,
    phrases: Vec<String>,
    keywords: Vec<String>,
    limit: usize,
    db_client: Option<Arc<Db>>,
    client: Arc<Client>,
) -> Result<(String, Vec<CaseResult>), (String, String)> {
    // term -> cases found for it, for keywords and phrases separately
    let (k_results, p_results): (HashMap<String, Vec<CaseResult>>, HashMap<String, Vec<CaseResult>>) = match court.as_str() {
        "ustavni" => {
            let (k_citations, p_citations) = scrapers::search_ustavni_citations(&client, &phrases, &keywords)
                .await
                .map_err(|e| (court.clone(), e.to_string()))?;

            // Citations are deduplicated by their space-free form, so a case found
            // by several terms is fetched only once
            let mut unique_cits = HashMap::new();
            let mut take_citations = |citations: HashMap<String, Vec<(String, String)>>| -> HashMap<String, Vec<String>> {
                citations
                    .into_iter()
                    .map(|(term, hrefs)| {
                        let norms = hrefs
                            .into_iter()
                            .take(limit)
                            .map(|(href, citation)| {
                                let norm = citation.replace(' ', "");
                                unique_cits.insert(norm.clone(), (href, citation));
                                norm
                            })
                            .collect();
                        (term, norms)
                    })
                    .collect()
            };
            let k_cit_map = take_citations(k_citations);
            let p_cit_map = take_citations(p_citations);

            let mut fetch_set = task::JoinSet::new();
            for (norm, (href, citation)) in unique_cits {
                let db = db_client.clone();
                let client = client.clone();
                fetch_set.spawn(async move {
                    find_or_scrape(db.as_deref(), &citation, scrapers::fetch_case_detail(&client, &href, &citation))
                        .await
                        .map(|case| (norm, case))
                        .map_err(|e| e.to_string())
                });
            }

            let mut final_cases = HashMap::new();
            while let Some(res) = fetch_set.join_next().await {
                match res {
                    Ok(Ok((norm, case))) => { final_cases.insert(norm, case); }
                    Ok(Err(e)) => warn!("Failed to fetch case: {}", e),
                    Err(e) => error!("Fetch task panicked: {}", e),
                }
            }

            let resolve = |cit_map: HashMap<String, Vec<String>>| {
                cit_map
                    .into_iter()
                    .map(|(term, norms)| {
                        let cases = norms.into_iter().filter_map(|n| final_cases.get(&n).cloned()).collect();
                        (term, cases)
                    })
                    .collect()
            };
            (resolve(k_cit_map), resolve(p_cit_map))
        }
        "nejvyssi" | "nejvyssi_spravni" => {
            let terms: Vec<String> = keywords.iter().take(limit)
                .chain(phrases.iter().take(limit))
                .cloned()
                .collect();

            let mut fetch_set = task::JoinSet::new();
            for term in terms {
                let db = db_client.clone();
                let client = client.clone();
                let court = court.clone();
                fetch_set.spawn(async move {
                    find_or_scrape(db.as_deref(), &term, scrape_case(&client, &court, &term))
                        .await
                        .map(|case| (term, case))
                        .map_err(|e| e.to_string())
                });
            }

            let mut final_cases = HashMap::new();
            while let Some(res) = fetch_set.join_next().await {
                match res {
                    Ok(Ok((term, case))) => { final_cases.insert(term, case); }
                    Ok(Err(e)) => warn!("Failed to fetch Supreme case: {}", e),
                    Err(e) => error!("Supreme fetch task panicked: {}", e),
                }
            }

            let resolve = |terms: Vec<String>| {
                terms
                    .into_iter()
                    .map(|term| {
                        let cases = final_cases.get(&term).cloned().into_iter().collect();
                        (term, cases)
                    })
                    .collect()
            };
            (resolve(keywords), resolve(phrases))
        }
        _ => {
            warn!("Unknown court type '{}' in search", court);
            return Err((court.clone(), format!("Unknown court type: {}", court)));
        }
    };

    let cases = k_results.into_values().chain(p_results.into_values()).flatten().collect();
    Ok((court, cases))
}

/// 422 response for a request the server cannot act on.
fn unprocessable(message: impl Into<String>) -> Response {
    let body = ErrorResponse { error: message.into() };
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}

/// 200 when nothing failed, 207 when some cases failed, 502 when everything failed.
/// Failures come from the court websites, hence 502 rather than 500.
fn response_status(success: bool, processed_count: usize) -> StatusCode {
    if success {
        StatusCode::OK
    } else if processed_count > 0 {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::BAD_GATEWAY
    }
}

const COURTS: [&str; 3] = ["ustavni", "nejvyssi", "nejvyssi_spravni"];

/// Maps a stored court name (key or full Czech name) to its court key.
fn court_key(soud: &str) -> &'static str {
    let soud = soud.to_lowercase();
    // "nejvyšší správní" has to be checked before "nejvyšší"
    if soud.contains("ústavní") || soud.contains("ustavni") {
        "ustavni"
    } else if soud.contains("nejvyšší správní") || soud.contains("spravni") {
        "nejvyssi_spravni"
    } else if soud.contains("nejvyšší") || soud.contains("nejvyssi") {
        "nejvyssi"
    } else {
        "other"
    }
}

async fn scrape_case(client: &Client, court: &str, case: &str) -> Result<CaseResult, BoxError> {
    match court {
        "nejvyssi" => scrape_nejvyssi(client, case).await,
        "nejvyssi_spravni" => scrape_nejvyssi_spravni(client, case).await,
        _ => scrape_ustavni(client, case).await,
    }
}

/// Writes the log as pretty JSON to `_LOGS/scraper/<timestamp>.json`.
fn save_log_file(log: &ScrapeLog) -> std::io::Result<()> {
    let dir_path = std::path::Path::new("_LOGS/scraper");
    std::fs::create_dir_all(dir_path)?;

    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let file_path = dir_path.join(format!("{}.json", timestamp));

    std::fs::write(&file_path, serde_json::to_string_pretty(log)?)?;

    info!("Saved scrape log to {:?}", file_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use utoipa::OpenApi;

    #[derive(OpenApi)]
    #[openapi(
        info(
            title = "rust-scraper",
            description = "Fetches Czech court decisions by case number or search term and stores them in PostgreSQL."
        ),
        servers((url = "http://localhost:8080", description = "Local `cargo run`")),
        paths(health_handler, scrape_handler, search_handler)
    )]
    struct ApiDoc;

    /// `openapi.json` is what GitHub Pages publishes. Regenerate it with
    /// `UPDATE_OPENAPI=1 cargo test openapi`.
    #[test]
    fn openapi_json_is_up_to_date() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json");
        let mut doc = ApiDoc::openapi();
        // utoipa copies the empty Cargo.toml license
        doc.info.license = None;
        let generated = doc.to_pretty_json().unwrap() + "\n";

        if env::var("UPDATE_OPENAPI").is_ok() {
            std::fs::write(path, &generated).unwrap();
            return;
        }

        let committed = std::fs::read_to_string(path).unwrap_or_default().replace("\r\n", "\n");
        assert!(
            committed == generated,
            "openapi.json is stale. Run `UPDATE_OPENAPI=1 cargo test openapi` and commit it."
        );
    }
}

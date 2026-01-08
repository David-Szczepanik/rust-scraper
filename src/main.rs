use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use lol_html::{element, text, HtmlRewriter, Settings};
use postgrest::Postgrest;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::task;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use url::Url;

const BASE_URL: &str = "https://nalus.usoud.cz/Search";

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
    let debug_mode = env::var("DEBUG").unwrap_or_else(|_| "1".to_string()) == "0";

    if debug_mode {
        info!("Running in DEBUG mode - executing hardcoded search cases");

        let debug_cases = vec![
            "I. ÚS 823/11".to_string(),
            "I. ÚS 1927/24".to_string(),
            "I. ÚS 1933/24".to_string(),
            "I. ÚS 367/03".to_string(),
            "III. ÚS 358/14".to_string(),
            "I. ÚS 3018/14".to_string(),
        ];

        let payload = ScrapeRequest {
            cases: debug_cases,
            task_id: "debug-task".to_string(),
        };

        let mut handles = vec![];

        for case_number in payload.cases.clone() {
            let state_clone = state.clone();
            let case = case_number.clone();

            let handle = task::spawn(async move {
                match scrape_case(&state_clone.http_client, &case).await {
                    Ok(result) => {
                        info!("Successfully scraped: {}", result.spisova_znacka);
                        Ok(result)
                    }
                    Err(e) => {
                        error!("Failed to scrape {}: {}", case, e);
                        Err(case)
                    }
                }
            });
            handles.push(handle);
        }

        let mut results = vec![];
        let mut failed_cases = vec![];

        for handle in handles {
            match handle.await {
                Ok(Ok(result)) => results.push(result),
                Ok(Err(failed_case)) => failed_cases.push(failed_case),
                Err(e) => {
                    error!("Task panicked: {}", e);
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

        for result in &results {
            info!(
                "Result: {} - {} - {}",
                result.spisova_znacka, result.ecli, result.datum_rozhodnuti
            );
        }

        if !failed_cases.is_empty() {
            error!("Failed cases: {:?}", failed_cases);
        }

        let response = ScrapeResponse {
            success: failed_cases.is_empty(),
            task_id: payload.task_id,
            results,
            failed_cases,
            message: "DEBUG mode completed".to_string(),
        };

        if let Err(e) = save_log_file(&response) {
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CaseResult {
    pub spisova_znacka: String,
    pub ecli: String,
    pub datum_rozhodnuti: String,
    pub popularni_nazev: String,
    pub url_adresa: String,
    pub abstrakt: String,
    pub pravni_veta: String,
    pub text_dokumentu: String,
}

use hash_ids::HashIds;

// judikatura table
#[derive(Serialize)]
pub struct DbCase {
    pub jud_id: String,
    pub spisova_znacka: String,
    pub ecli: String,
    pub datum_rozhodnuti: String,
    pub popularni_nazev: String,
    pub url_adresa: String,
    pub abstrakt: String,
    pub pravni_veta: String,
    pub text_dokumentu: String,
}

impl From<CaseResult> for DbCase {
    fn from(c: CaseResult) -> Self {
        Self {
            jud_id: generate_jud_id(&c.spisova_znacka),
            spisova_znacka: c.spisova_znacka,
            ecli: c.ecli,
            datum_rozhodnuti: c.datum_rozhodnuti,
            popularni_nazev: c.popularni_nazev,
            url_adresa: c.url_adresa,
            abstrakt: c.abstrakt,
            pravni_veta: c.pravni_veta,
            text_dokumentu: c.text_dokumentu,
        }
    }
}

#[derive(Deserialize)]
pub struct ScrapeRequest {
    pub cases: Vec<String>,
    pub task_id: String,
}

#[derive(Serialize)]
pub struct ScrapeResponse {
    pub success: bool,
    pub task_id: String,
    pub results: Vec<CaseResult>,
    pub failed_cases: Vec<String>,
    pub message: String,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

#[derive(Clone)]
pub struct AppState {
    pub http_client: Client,
    pub supabase_client: Option<Arc<Postgrest>>,
}

impl AppState {
    pub fn new() -> Result<Self, String> {
        let http_client = Client::builder()
            .cookie_store(true)
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

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
            http_client,
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
    info!(
        "Received scrape request for {} cases, task_id: {}",
        payload.cases.len(),
        payload.task_id
    );

    let mut handles = vec![];

    // spawn concurrent scraping tasks
    for case_number in payload.cases.clone() {
        let state_clone = state.clone();
        let case = case_number.clone();

        let handle = task::spawn(async move {
            match scrape_case(&state_clone.http_client, &case).await {
                Ok(result) => {
                    info!("Successfully scraped: {}", result.spisova_znacka);
                    Ok(result)
                }
                Err(e) => {
                    error!("Failed to scrape {}: {}", case, e);
                    Err(case)
                }
            }
        });
        handles.push(handle);
    }

    let mut results = vec![];
    let mut failed_cases = vec![];

    for handle in handles {
        match handle.await {
            Ok(Ok(result)) => results.push(result),
            Ok(Err(failed_case)) => failed_cases.push(failed_case),
            Err(e) => {
                error!("Task panicked: {}", e);
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

    let response = ScrapeResponse {
            success,
            task_id: payload.task_id,
            results,
            failed_cases,
            message: format!(
                "Processed {} cases, {} failed",
                processed_count,
                payload.cases.len() - processed_count
            ),
        };

    if let Err(e) = save_log_file(&response) {
        error!("Failed to save log file: {}", e);
    }

    (status, Json(response))
}

fn save_log_file(response: &ScrapeResponse) -> std::io::Result<()> {
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

// scraping
async fn scrape_case(
    client: &Client,
    search_query: &str,
) -> Result<CaseResult, Box<dyn std::error::Error + Send + Sync>> {
    let search_url = format!("{}/Search.aspx", BASE_URL);

    // 1. fetch search page
    let initial_html = client.get(&search_url).send().await?.text().await?;
    let mut form_data = extract_hidden_fields(&initial_html)?;

    // 2. submit search
    form_data.insert(
        "ctl00$MainContent$citace".to_string(),
        search_query.to_string(),
    );
    form_data.insert("ctl00$MainContent$nalezy".to_string(), "on".to_string());
    form_data.insert("ctl00$MainContent$usneseni".to_string(), "on".to_string());
    form_data.insert(
        "ctl00$MainContent$but_search".to_string(),
        "Vyhledat".to_string(),
    );

    let response = client.post(&search_url).form(&form_data).send().await?;
    let status = response.status();
    let result_page_html = response.text().await?;
    info!("Search request for '{}' returned status: {}, content length: {} bytes", search_query, status, result_page_html.len());

    // 3. find detail link
    let target_href = find_result_href(&result_page_html, search_query).ok_or_else(|| {
        let msg = format!("Link not found for {}", search_query);
        Box::<dyn std::error::Error + Send + Sync>::from(msg)
    })?;

    let detail_url = format!("{}/{}", BASE_URL, target_href);

    // 4. fetch detail page
    let detail_html = client.get(&detail_url).send().await?.text().await?;
    let (ecli, datum, popularni_nazev) = extract_metadata(&detail_html);
    let text_dokumentu = extract_document_text(&detail_html);

    // 5. fetch Abstract separately if "ShowAbstrakt" button exists
    let (abstrakt, pravni_veta) = if has_show_abstrakt(&detail_html) {
        let parsed_url = Url::parse(&detail_url)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        let id_param = parsed_url
            .query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.to_string())
            .ok_or("Could not find 'id' parameter in detail URL")?;

        let abstrakt_url = format!("{}/Abstrakt.aspx?id={}", BASE_URL, id_param);
        let abstrakt_html = client.get(&abstrakt_url).send().await?.text().await?;

        extract_abstrakt_content(&abstrakt_html)
    } else {
        (String::new(), String::new())
    };

    Ok(CaseResult {
        spisova_znacka: search_query.to_string(),
        ecli,
        datum_rozhodnuti: datum,
        popularni_nazev,
        url_adresa: detail_url,
        abstrakt,
        pravni_veta,
        text_dokumentu,
    })
}

fn extract_document_text(html: &str) -> String {
    let text = Arc::new(Mutex::new(String::new()));
    let text_c = text.clone();
    let text_br = text.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                text!("#uc_vytah_cellContent", move |t| {
                    text_c.lock().unwrap().push_str(t.as_str());
                    Ok(())
                }),
                element!("#uc_vytah_cellContent br", move |_| {
                    text_br.lock().unwrap().push_str("\n");
                    Ok(())
                }),
            ],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );
    let _ = rewriter.write(html.as_bytes());
    let _ = rewriter.end();

    let result = text.lock().unwrap().trim().to_string();
    result
}

fn generate_jud_id(spisova_znacka: &str) -> String {
    let clean_znacka = spisova_znacka.trim();
    info!(
        "Looking for query: '{}'",
        clean_znacka
    );
    let hasher = HashIds::builder()
        .with_salt(clean_znacka)
        .with_min_length(8)
        .finish();
    hasher.encode(&[1])
}

async fn upload_to_supabase(
    client: &Postgrest,
    results: &[CaseResult],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let db_cases: Vec<DbCase> = results.iter().cloned().map(DbCase::from).collect();
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

fn extract_hidden_fields(
    html: &str,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
    let values = Arc::new(Mutex::new(HashMap::new()));
    let values_clone = values.clone();
    let target_fields = [
        "__VIEWSTATE",
        "__EVENTVALIDATION",
        "__VIEWSTATEGENERATOR",
        "__PREVIOUSPAGE",
    ];

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![element!("input[type=hidden]", move |el| {
                if let Some(name) = el.get_attribute("name") {
                    if target_fields.contains(&name.as_str()) {
                        let value = el.get_attribute("value").unwrap_or_default();
                        values_clone.lock().unwrap().insert(name, value);
                    }
                }
                Ok(())
            })],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );
    rewriter
        .write(html.as_bytes())
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    rewriter
        .end()
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    let result = Arc::try_unwrap(values)
        .map_err(|_| Box::<dyn std::error::Error + Send + Sync>::from("Failed to unwrap Arc"))?
        .into_inner()
        .map_err(|_| Box::<dyn std::error::Error + Send + Sync>::from("Failed to unlock Mutex"))?;
    Ok(result)
}

fn find_result_href(html: &str, query: &str) -> Option<String> {
    let candidates = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let candidates_clone_el = candidates.clone();
    let candidates_clone_text = candidates.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("a.resultData0", move |el| {
                    let href = el.get_attribute("href").unwrap_or_default();
                    candidates_clone_el
                        .lock()
                        .unwrap()
                        .push((href, String::new()));
                    Ok(())
                }),
                text!("a.resultData0", move |t| {
                    if let Some(last) = candidates_clone_text.lock().unwrap().last_mut() {
                        last.1.push_str(t.as_str());
                    }
                    Ok(())
                }),
            ],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );
    let _ = rewriter.write(html.as_bytes());
    let _ = rewriter.end();

    let candidates = candidates.lock().unwrap();

    let normalize_base = |s: &str| -> String {
        s.to_lowercase().replace("ú", "u").replace("ů", "u")
    };

    let strip_whitespace = |s: &str| -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    };

    let normalize_strict = |s: &str| -> String {
        strip_whitespace(&normalize_base(s))
    };

    let normalize_loose = |s: &str| -> String {
        let base = normalize_base(s);
        let suffix = if let Some(idx) = base.find("us") {
            &base[idx..]
        } else {
            &base
        };
        strip_whitespace(suffix)
    };

    let query_strict = normalize_strict(query);
    let query_loose = normalize_loose(query);

    info!(
        "Looking for query: '{}'. Strict: '{}', Loose: '{}'",
        query, query_strict, query_loose
    );

    for (href, text) in candidates.iter() {
        let text_strict = normalize_strict(text);

        if text_strict.starts_with(&query_strict) {
            let remainder = &text_strict[query_strict.len()..];

            if remainder.is_empty() || !remainder.chars().next().unwrap().is_alphanumeric() {
                return Some(href.clone());
            }
        }
    }

    for (href, text) in candidates.iter() {
        let text_loose = normalize_loose(text);

        if !query_loose.is_empty() && text_loose.starts_with(&query_loose) {
             let remainder = &text_loose[query_loose.len()..];
             if remainder.is_empty() || !remainder.chars().next().unwrap().is_alphanumeric() {
                 info!("Found loose match for '{}': '{}'", query, text);
                 return Some(href.clone());
             }
        }
    }

    None
}

#[derive(Clone, Copy, PartialEq)]
enum MetaField {
    Ecli,
    Datum,
    PopularTitle,
}

fn extract_metadata(html: &str) -> (String, String, String) {
    let next_capture = Arc::new(Mutex::new(None::<MetaField>));
    let current_capture = Arc::new(Mutex::new(None::<MetaField>));
    let results = Arc::new(Mutex::new((String::new(), String::new(), String::new())));

    let next_capture_c = next_capture.clone();
    let next_capture_c2 = next_capture.clone();
    let current_capture_c = current_capture.clone();
    let current_capture_c2 = current_capture.clone();
    let results_c = results.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("table.recordCardTable td", move |_| {
                    let mut next = next_capture_c.lock().unwrap();
                    let mut curr = current_capture_c.lock().unwrap();
                    *curr = next.take();
                    Ok(())
                }),
                text!("table.recordCardTable td", move |t| {
                    let text = t.as_str();
                    let curr = *current_capture_c2.lock().unwrap();
                    match curr {
                        Some(MetaField::Ecli) => results_c.lock().unwrap().0.push_str(text),
                        Some(MetaField::Datum) => results_c.lock().unwrap().1.push_str(text),
                        Some(MetaField::PopularTitle) => results_c.lock().unwrap().2.push_str(text),
                        None => {}
                    }
                    if text.contains("Identifikátor evropské judikatury") {
                        *next_capture_c2.lock().unwrap() = Some(MetaField::Ecli);
                    } else if text.contains("Datum rozhodnutí") {
                        *next_capture_c2.lock().unwrap() = Some(MetaField::Datum);
                    } else if text.contains("Populární název") {
                        *next_capture_c2.lock().unwrap() = Some(MetaField::PopularTitle);
                    }
                    Ok(())
                }),
            ],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );
    let _ = rewriter.write(html.as_bytes());
    let _ = rewriter.end();

    let guard = results.lock().unwrap();
    let raw_datum = guard.1.trim().to_string();
    let clean_datum = raw_datum.replace("Forma rozhodnutí", "").trim().to_string();

    (
        guard.0.trim().to_string(),
        clean_datum,
        guard.2.trim().to_string(),
    )
}

fn has_show_abstrakt(html: &str) -> bool {
    let found = Arc::new(Mutex::new(false));
    let found_c = found.clone();
    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![element!("input[name='ShowAbstrakt']", move |_| {
                *found_c.lock().unwrap() = true;
                Ok(())
            })],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );
    let _ = rewriter.write(html.as_bytes());
    let _ = rewriter.end();
    let result = *found.lock().unwrap();
    result
}

fn extract_abstrakt_content(html: &str) -> (String, String) {
    let abstract_text = Arc::new(Mutex::new(String::new()));
    let legal_text = Arc::new(Mutex::new(String::new()));

    let abstract_text_c = abstract_text.clone();
    let legal_text_c = legal_text.clone();

    let abstract_text_br = abstract_text.clone();
    let legal_text_br = legal_text.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                text!(".abstractContent", move |t| {
                    abstract_text_c.lock().unwrap().push_str(t.as_str());
                    Ok(())
                }),
                text!(".legalSentenceContent", move |t| {
                    legal_text_c.lock().unwrap().push_str(t.as_str());
                    Ok(())
                }),
                element!(".abstractContent br", move |_| {
                    abstract_text_br.lock().unwrap().push_str("<br>");
                    Ok(())
                }),
                element!(".legalSentenceContent br", move |_| {
                    legal_text_br.lock().unwrap().push_str("<br>");
                    Ok(())
                }),
            ],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );
    let _ = rewriter.write(html.as_bytes());
    let _ = rewriter.end();

    let raw_abs = abstract_text.lock().unwrap().trim().to_string();
    let raw_legal = legal_text.lock().unwrap().trim().to_string();

    let final_abs = if raw_abs.contains("Abstrakt není k dispozici") {
        String::new()
    } else {
        raw_abs
    };
    let final_legal = if raw_legal.contains("Právní věta není k dispozici") {
        String::new()
    } else {
        raw_legal
    };

    (final_abs, final_legal)
}

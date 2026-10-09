use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// One court decision, either scraped from a court website or found in the database.
/// A case found in the database only has `spisova_znacka` and `soud` filled in.
#[derive(Serialize, Debug, Clone, Default)]
pub struct CaseResult {
    /// Case number, e.g. "Pl. ÚS 19/93".
    pub spisova_znacka: String,
    pub ecli: String,
    pub datum_rozhodnuti: String,
    /// Only the Constitutional Court (ustavni) has popular names; `None` elsewhere.
    pub popularni_nazev: Option<String>,
    /// Court key ("ustavni", "nejvyssi", "nejvyssi_spravni"). NSS adds the senate as a second entry.
    pub soud: Vec<String>,
    pub url_adresa: String,
    /// Only the Constitutional Court has abstracts; `None` elsewhere.
    pub abstrakt: Option<String>,
    pub pravni_veta: String,
    /// Only the Supreme Court (nejvyssi) has categories; `None` elsewhere.
    pub kategorie: Option<String>,
    pub text_dokumentu: String,
    pub found_in_db: bool,
}

impl CaseResult {
    /// A case that is already in the database; only its number and court are known.
    pub fn from_db(spisova_znacka: String, soud: String) -> Self {
        Self {
            spisova_znacka,
            soud: vec![soud],
            found_in_db: true,
            ..Default::default()
        }
    }
}

/// A log entry. A case that was already in the database is logged only by its number.
#[derive(Serialize)]
#[serde(untagged)]
pub enum LogResult {
    Scraped(CaseResult),
    InDatabase {
        spisova_znacka: String,
        found_in_db: bool,
    },
}

impl From<CaseResult> for LogResult {
    fn from(case: CaseResult) -> Self {
        if case.found_in_db {
            LogResult::InDatabase {
                spisova_znacka: case.spisova_znacka,
                found_in_db: true,
            }
        } else {
            LogResult::Scraped(case)
        }
    }
}

/// Case numbers to fetch, per court. Each court list is optional.
#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct ScrapeRequest {
    /// Caller's id for this request, echoed back and written to the log.
    #[schema(example = "123")]
    pub task_id: Option<String>,
    /// Constitutional Court case numbers.
    #[schema(example = json!(["Pl. ÚS 19/93"]))]
    pub ustavni: Option<Vec<String>>,
    /// Supreme Court case numbers.
    #[schema(example = json!(["3 Tdo 706/2024"]))]
    pub nejvyssi: Option<Vec<String>>,
    /// Supreme Administrative Court case numbers.
    #[schema(example = json!(["1 As 100/2020"]))]
    pub nejvyssi_spravni: Option<Vec<String>>,
    /// Takes only the first case per court. Also on when the `DEBUG` env var is `1`.
    pub debug_mode: Option<bool>,
    /// Maximum number of cases taken from each court list. Default 5.
    #[schema(example = 5)]
    pub limit: Option<usize>,
}

/// Query of `GET /cases/search`. Repeat a parameter to pass several values:
/// `?court=ustavni&court=nejvyssi&keyword=restituce`.
#[derive(Serialize, Deserialize, Debug, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchRequest {
    /// Caller's id for this request, echoed back and written to the log.
    pub task_id: Option<String>,
    /// Court key to search in: `ustavni`, `nejvyssi` or `nejvyssi_spravni`. Required, repeatable.
    #[serde(default, rename = "court")]
    pub courts: Vec<String>,
    /// Phrase to search for. Repeatable.
    #[serde(default, rename = "phrase")]
    pub phrases: Vec<String>,
    /// Keyword to search for. Repeatable. At least one phrase or keyword is required.
    #[serde(default, rename = "keyword")]
    pub keywords: Vec<String>,
    /// Maximum number of hits per term. Default 5.
    pub limit: Option<usize>,
}

/// Case numbers found, grouped by court key, and the failures.
#[derive(Serialize, ToSchema)]
pub struct CasesResponse {
    /// The `task_id` from the request, if one was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "123")]
    pub task_id: Option<String>,
    pub scraped_count: usize,
    /// Court key -> case numbers.
    #[schema(example = json!({ "ustavni": ["Pl. ÚS 19/93"], "nejvyssi": ["3 Tdo 706/2024"] }))]
    pub results: HashMap<String, Vec<String>>,
    /// Case number (or court, for search) -> error message.
    #[schema(example = json!({ "1 As 100/2020": "Case not found" }))]
    pub failed: HashMap<String, String>,
}

/// Body of every 4xx response.
#[derive(Serialize, ToSchema)]
pub struct ErrorResponse {
    #[schema(example = "no court given")]
    pub error: String,
}

/// Written to `_LOGS/scraper/<timestamp>.json` after each request.
#[derive(Serialize)]
pub struct ScrapeLog {
    pub success: bool,
    pub task_id: String,
    pub scraped_count: usize,
    pub results: Vec<LogResult>,
    /// Case number (or search term) -> error message.
    pub failed_cases: HashMap<String, String>,
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

/// Splits "3063/2020 #1" into ("3063", "2020", " #1"): the part before the last '/',
/// the year digits right after it, and the rest. Whitespace after the '/' is skipped.
fn split_year(input: &str) -> Option<(&str, &str, &str)> {
    let pos = input.rfind('/')?;
    let after_slash = input[pos + 1..].trim_start();
    let digits_len = after_slash.chars().take_while(|c| c.is_ascii_digit()).count();
    let (digits, rest) = after_slash.split_at(digits_len);
    Some((&input[..pos], digits, rest))
}

/// Shortens a 4-digit year after the last '/' to 2 digits, so "3063/20", "3063/2020"
/// and "3063/20 #1" all compare equal after normalization.
pub fn shorten_year(input: &str) -> String {
    match split_year(input) {
        Some((head, digits, rest)) if digits.len() == 4 => format!("{}/{}{}", head, &digits[2..], rest),
        _ => input.to_string(),
    }
}

/// The input plus the same case number with the other year form:
/// "1/93" -> ["1/93", "1/1993"], "1/2010" -> ["1/2010", "1/10"].
/// 2-digit years above 50 are 19xx, the rest 20xx.
pub fn get_year_variants(input: &str) -> Vec<String> {
    let mut result = vec![input.to_string()];

    if let Some((head, digits, rest)) = split_year(input) {
        let rest = rest.trim_end();
        match digits.len() {
            2 => {
                let year: u32 = digits.parse().unwrap_or(0);
                let full_year = if year > 50 { 1900 + year } else { 2000 + year };
                result.push(format!("{}/{}{}", head, full_year, rest));
            }
            4 => result.push(format!("{}/{}{}", head, &digits[2..], rest)),
            _ => {}
        }
    }

    result
}

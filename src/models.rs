use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct CaseResult {
    pub spisova_znacka: String,
    pub ecli: String,
    pub datum_rozhodnuti: String,
    pub popularni_nazev: Option<String>,
    pub soud: Vec<String>,
    pub url_adresa: String,
    pub abstrakt: Option<String>,
    pub pravni_veta: String,
    pub kategorie: Option<String>,
    pub text_dokumentu: String,
    #[serde(default)]
    pub found_in_db: bool,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum LogResult {
    Scraped(CaseResult),
    InDatabase {
        spisova_znacka: String,
        found_in_db: bool,
    },
}

#[derive(Serialize, Clone)]
pub struct ScrapedCase {
    pub spisova_znacka: String,
}

impl From<CaseResult> for ScrapedCase {
    fn from(c: CaseResult) -> Self {
        Self {
            spisova_znacka: c.spisova_znacka,
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct ScrapeRequest {
    pub task_id: String,
    pub ustavni: Option<Vec<String>>,
    pub nejvyssi: Option<Vec<String>>,
    pub nejvyssi_spravni: Option<Vec<String>>,
    pub debug_mode: Option<bool>,
    pub limit: Option<usize>,
}

#[derive(Deserialize, Debug)]
pub struct SearchRequest {
    pub task_id: String,
    pub courts: Option<Vec<String>>,
    pub phrases: Option<Vec<String>>,
    pub keywords: Option<Vec<String>>,
    pub limit: Option<usize>,
}

#[derive(Serialize, Default, Clone)]
pub struct CourtResults {
    pub keywords: std::collections::HashMap<String, Vec<ScrapedCase>>,
    pub phrases: std::collections::HashMap<String, Vec<ScrapedCase>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cases: Vec<ScrapedCase>,
}

#[derive(Serialize)]
pub struct ScrapeResponse {
    pub task_id: String,
    pub scraped_count: usize,
    pub results: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Serialize)]
pub struct ScrapeLog {
    pub success: bool,
    pub task_id: String,
    pub scraped_count: usize,
    pub results: Vec<LogResult>,
    pub failed_cases: std::collections::HashMap<String, String>,
    pub message: String,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

/// Normalizes a year after the last '/' to its 2-digit short form so that
/// "3063/20", "3063/2020", and "3063/20 #1" all compare equal after normalization.
pub fn expand_year(input: &str) -> String {
    if let Some(pos) = input.rfind('/') {
        let after_slash = input[pos + 1..].trim_start();
        let digits: String = after_slash.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() == 4 {
            let short = &digits[2..];
            let rest = &after_slash[digits.len()..];
            return format!("{}/{}{}", &input[..pos], short, rest);
        }
    }
    input.to_string()
}

pub fn get_year_variants(input: &str) -> Vec<String> {
    let mut result = vec![input.to_string()];
    
    if let Some(pos) = input.rfind('/') {
        let after_slash = input[pos + 1..].trim();
        let digits: String = after_slash.chars().take_while(|c| c.is_ascii_digit()).collect();
        let rest: String = after_slash.chars().skip(digits.len()).collect();
        
        if digits.len() == 2 {
            let year = digits.parse::<u32>().unwrap_or(0);
            let century = if year > 50 { 1900 } else { 2000 };
            let full_year = century + year;
            let expanded_str = format!("{}/{}{}", &input[..pos], full_year, rest);
            if !result.contains(&expanded_str) {
                result.push(expanded_str);
            }
        } else if digits.len() == 4 {
            let short_year = &digits[2..];
            let short_str = format!("{}/{}{}", &input[..pos], short_year, rest);
            if !result.contains(&short_str) {
                result.push(short_str);
            }
        }
    }
    
    result
}

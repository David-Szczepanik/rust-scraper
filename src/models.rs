use serde::{Deserialize, Serialize};
use hash_ids::HashIds;
use tracing::info;

#[derive(Serialize, Deserialize, Debug, Clone)]
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
}

#[derive(Serialize)]
pub struct ScrapedCase {
    pub spisova_znacka: String,
    pub kategorie: Option<String>,
    pub text_dokumentu: String,
}

impl From<CaseResult> for ScrapedCase {
    fn from(c: CaseResult) -> Self {
        Self {
            spisova_znacka: c.spisova_znacka,
            kategorie: c.kategorie.clone(),
            text_dokumentu: c.text_dokumentu,
        }
    }
}

// judikatura table
#[derive(Serialize)]
pub struct DbCase {
    pub jud_id: String,
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
}

impl From<CaseResult> for DbCase {
    fn from(c: CaseResult) -> Self {
        Self {
            jud_id: generate_jud_id(&c.spisova_znacka),
            spisova_znacka: c.spisova_znacka,
            ecli: c.ecli,
            datum_rozhodnuti: c.datum_rozhodnuti,
            popularni_nazev: c.popularni_nazev.clone(),
            soud: c.soud.clone(),
            url_adresa: c.url_adresa,
            abstrakt: c.abstrakt.clone(),
            pravni_veta: c.pravni_veta,
            kategorie: c.kategorie,
            text_dokumentu: c.text_dokumentu,
        }
    }
}

pub fn generate_jud_id(spisova_znacka: &str) -> String {
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

#[derive(Deserialize, Debug)]
pub struct ScrapeRequest {
    pub task_id: String,
    pub ustavni: Option<Vec<String>>,
    pub nejvyssi: Option<Vec<String>>,
    pub nejvyssi_spravni: Option<Vec<String>>,
    pub debug_mode: Option<bool>,
}

#[derive(Serialize)]
pub struct ScrapeResponse {
    pub task_id: String,
    pub scraped_count: usize,
    pub results: Vec<ScrapedCase>,
}

#[derive(Serialize)]
pub struct ScrapeLog {
    pub success: bool,
    pub task_id: String,
    pub scraped_count: usize,
    pub results: Vec<CaseResult>,
    pub failed_cases: std::collections::HashMap<String, String>,
    pub message: String,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

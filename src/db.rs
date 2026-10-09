use std::collections::HashMap;
use std::future::Future;
use tokio_postgres::NoTls;
use tracing::{error, info};

use crate::models::CaseResult;
use crate::scrapers::BoxError;

pub type Db = tokio_postgres::Client;

pub async fn connect_db(database_url: &str) -> Result<Db, String> {
    // Log only host part
    let masked_url = database_url.split('@').last().unwrap_or("unknown");
    info!("Connecting to database at {}...", masked_url);
    let (client, connection) = tokio_postgres::connect(database_url, NoTls)
        .await
        .map_err(|e| format!("Failed to connect to database: {}", e))?;
    spawn_connection(connection);
    Ok(client)
}

/// Postgres connection in background
// tokio_postgres needs this for client to work
fn spawn_connection(
    connection: impl Future<Output = Result<(), tokio_postgres::Error>> + Send + 'static,
) {
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            error!("Database connection error: {}", e);
        }
    });
}

pub async fn find_or_scrape(
    db: Option<&Db>,
    case: &str,
    scrape: impl Future<Output = Result<CaseResult, BoxError>>,
) -> Result<CaseResult, BoxError> {
    if let Some(db) = db {
        if let Ok(Some(existing)) = get_case_from_db(db, case).await {
            info!("Case {} found in database, skipping scrape", case);
            return Ok(existing);
        }
    }
    info!("Case {} not in database, scraping...", case);
    let new_case = scrape.await?;
    save_to_db(db, &new_case).await;
    Ok(new_case)
}

pub async fn get_cases_from_db_bulk(
    db: &Db,
    spisova_znacky: &[String],
) -> Result<HashMap<String, (String, String)>, BoxError> {
    let rows = db.query(
        "SELECT j.spisova_znacka, j.soud, s AS original_query
         FROM judikatura j
         JOIN unnest($1::text[]) AS s
           ON j.spisova_znacka_norm
              LIKE LOWER(REPLACE(REPLACE(immutable_unaccent(SPLIT_PART(s, ' - ', 1)), ' ', ''), '.', '')) || '%'",
        &[&spisova_znacky],
    ).await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let soud: Option<String> = row.get(1);
            (row.get(2), (row.get(0), soud.unwrap_or_else(|| "unknown".to_string())))
        })
        .collect())
}

async fn get_case_from_db(db: &Db, spisova_znacka: &str) -> Result<Option<CaseResult>, BoxError> {
    let mut results = get_cases_from_db_bulk(db, &[spisova_znacka.to_string()]).await?;
    Ok(results.remove(spisova_znacka).map(|(id, soud)| CaseResult::from_db(id, soud)))
}

async fn upsert_to_db(db: &Db, results: &[CaseResult]) -> Result<(), BoxError> {
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

pub async fn save_to_db(db: Option<&Db>, case: &CaseResult) {
    if let Some(db) = db {
        let _ = upsert_to_db(db, std::slice::from_ref(case)).await;
    }
}

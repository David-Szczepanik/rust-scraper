use reqwest::Client;
use std::cell::RefCell;
use lol_html::{element, text};
use crate::models::CaseResult;
use tracing::{info, warn};
use super::common::{decode, fetch_text, rewrite, BoxError};

const BASE_URL: &str = "https://rozhodnuti.nsoud.cz";

pub async fn scrape_nejvyssi(
    client: &Client,
    search_query: &str,
) -> Result<CaseResult, BoxError> {
    info!("Scraping Supreme Court (NS) for: {}", search_query);

    // "3 Tdo 706/2024" -> senat=3, druh=Tdo, cislo=706, rok=2024
    let parts: Vec<&str> = search_query.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(format!("Invalid search query format: {}", search_query).into());
    }

    let senat = parts[0];
    let druh = parts[1];
    let rest = parts[2];
    let mut sub_parts = rest.split('/');
    let (Some(cislo), Some(rok)) = (sub_parts.next(), sub_parts.next()) else {
        return Err(format!("Invalid case number format in: {}", rest).into());
    };

    let query = format!("[spzn1]={} AND [spzn2]={} AND [spzn3]={} AND [spzn4]={}", senat, druh, cislo, rok);
    let search_url = format!(
        "{}/judikatura/judikatura_ns.nsf/WebSearch?SearchView&Query={}&SearchMax=1000&SearchOrder=4&Start=0&Count=20&pohled=1",
        BASE_URL,
        urlencoding::encode(&query)
    );

    info!("Searching at: {}", search_url);

    let response = client.get(&search_url).send().await?;
    let status = response.status();
    let results_html = response.text().await?;

    let Some(href) = find_detail_link(&results_html) else {
        warn!("Case not found: {}", search_query);
        return Err(format!("Case {} not found in NS database. Status: {}", search_query, status).into());
    };

    let full_detail_url = if href.starts_with('/') {
        format!("{}{}", BASE_URL, href)
    } else {
        format!("{}/judikatura/judikatura_ns.nsf/{}", BASE_URL, href)
    };
    info!("Found detail URL: {}", full_detail_url);

    let metadata = extract_ns_metadata(&fetch_text(client, &full_detail_url).await?);

    Ok(CaseResult {
        soud: vec!["nejvyssi".to_string()],
        spisova_znacka: search_query.to_string(),
        ecli: metadata.ecli,
        datum_rozhodnuti: metadata.datum,
        popularni_nazev: None,
        url_adresa: full_detail_url,
        abstrakt: None,
        pravni_veta: metadata.pravni_veta,
        kategorie: Some(metadata.kategorie),
        text_dokumentu: metadata.text_dokumentu,
        found_in_db: false,
    })
}

/// First `a.odk` link, or failing that the first OpenDocument link that is not a PDF/RTF.
fn find_detail_link(html: &str) -> Option<String> {
    let link = RefCell::new(None::<String>);
    let set_first = |href: String| {
        link.borrow_mut().get_or_insert(href);
    };

    let _ = rewrite(html, vec![
        element!("a.odk", |el| {
            if let Some(href) = el.get_attribute("href") {
                set_first(href);
            }
            Ok(())
        }),
        element!("a", |el| {
            if let Some(href) = el.get_attribute("href") {
                if href.contains("OpenDocument") && !href.contains(".pdf") && !href.contains(".rtf") {
                    set_first(href);
                }
            }
            Ok(())
        }),
    ]);

    link.into_inner()
}

#[derive(Default)]
struct NsMetadata {
    ecli: String,
    datum: String,
    pravni_veta: String,
    kategorie: String,
    text_dokumentu: String,
}

fn extract_ns_metadata(html: &str) -> NsMetadata {
    let meta = RefCell::new(NsMetadata::default());
    let current_label = RefCell::new(String::new());

    // Each row is a td.left-part label followed by a td.right-part value
    let _ = rewrite(html, vec![
        element!("td.left-part", |_| {
            current_label.borrow_mut().clear();
            Ok(())
        }),
        text!("td.left-part", |t| {
            current_label.borrow_mut().push_str(t.as_str());
            Ok(())
        }),
        text!("td.right-part", |t| {
            let label = current_label.borrow().to_lowercase();
            let mut meta = meta.borrow_mut();

            if label.contains("ecli") {
                meta.ecli.push_str(t.as_str());
            } else if label.contains("datum rozhodnutí") {
                meta.datum.push_str(t.as_str());
            } else if label.contains("heslo") {
                // Not stored, but a "heslo" label must not fall through to the labels below
            } else if label.contains("právní věta") {
                meta.pravni_veta.push_str(t.as_str());
            } else if label.contains("kategorie") {
                meta.kategorie.push_str(t.as_str());
            }
            Ok(())
        }),
        text!("div[style*=\"text-align:justify\"]", |t| {
            meta.borrow_mut().text_dokumentu.push_str(t.as_str());
            Ok(())
        }),
    ]);

    let meta = meta.into_inner();
    NsMetadata {
        ecli: decode(&meta.ecli),
        datum: decode(&meta.datum),
        pravni_veta: decode(&meta.pravni_veta),
        kategorie: decode(&meta.kategorie),
        text_dokumentu: decode(&meta.text_dokumentu),
    }
}

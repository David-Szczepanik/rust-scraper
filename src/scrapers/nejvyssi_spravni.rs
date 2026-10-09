use reqwest::Client;
use std::cell::RefCell;
use std::collections::HashMap;
use lol_html::{element, text};
use scraper::{Html, Selector, Node};
use crate::models::CaseResult;
use tracing::{info, warn};
use super::common::{decode, fetch_text, rewrite, BoxError};

const BASE_URL: &str = "https://vyhledavac.nssoud.cz";

fn extract_nss_form_fields(html: &str) -> HashMap<String, String> {
    let fields = RefCell::new(HashMap::new());
    let _ = rewrite(html, vec![element!("input", |el| {
        if let Some(name) = el.get_attribute("name") {
            let value = el.get_attribute("value").unwrap_or_default();
            fields.borrow_mut().insert(name, value);
        }
        Ok(())
    })]);
    fields.into_inner()
}

pub async fn scrape_nejvyssi_spravni(
    client: &Client,
    search_query: &str,
) -> Result<CaseResult, BoxError> {
    // Drop anything after " - " (usually a page number, like "1 As 118/2012 - 41")
    let cleaned_query = search_query
        .split_once(" - ")
        .map_or(search_query, |(query, _)| query)
        .trim();

    info!("Scraping Supreme Administrative Court (NSS) for: {} (cleaned: {})", search_query, cleaned_query);

    // The search form needs the page's hidden fields, including __RequestVerificationToken
    let search_page_url = format!("{}/", BASE_URL);
    let mut form_data = extract_nss_form_fields(&fetch_text(client, &search_page_url).await?);

    form_data.insert(
        "vyhledavaciSekce[0].vyhledavaciPodminka[1].vyhledavaciPodminkaHodnota[0].HodnotaText".to_string(),
        cleaned_query.to_string()
    );
    form_data.insert("Formular".to_string(), "1".to_string());

    let response = client.post(&search_page_url)
        .form(&form_data)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("NSS search failed with status: {}", response.status()).into());
    }

    let results_html = response.text().await?;

    let Some(id) = find_nss_doc_id(&results_html) else {
        warn!("NSS case not found: {}", search_query);
        return Err(format!("Case {} not found in NSS database", search_query).into());
    };
    info!("Found NSS document ID: {}", id);

    // The Text endpoint gives cleaner document text than the Html one
    let text_url = format!("{}/DokumentOriginal/Text/{}", BASE_URL, id);
    let detail_url = format!("{}/DokumentDetail/Index/{}", BASE_URL, id);

    let doc_text = fetch_text(client, &text_url).await?;
    let detail_html = fetch_text(client, &detail_url).await?;

    let metadata = extract_nss_metadata(&detail_html);

    Ok(CaseResult {
        soud: vec!["nejvyssi_spravni".to_string(), metadata.soud_senat],
        spisova_znacka: cleaned_query.to_string(),
        ecli: metadata.ecli,
        datum_rozhodnuti: metadata.datum,
        popularni_nazev: None,
        url_adresa: detail_url,
        abstrakt: None,
        pravni_veta: metadata.pravni_veta,
        kategorie: None,
        text_dokumentu: clean_html_text(&doc_text),
        found_in_db: false,
    })
}

/// The last document ID on the results page; later matches overwrite earlier ones.
fn find_nss_doc_id(html: &str) -> Option<String> {
    let doc_id = RefCell::new(None::<String>);

    let _ = rewrite(html, vec![
        element!("a", |el| {
            if let Some(href) = el.get_attribute("href") {
                if href.contains("/DokumentOriginal/Html/") {
                    *doc_id.borrow_mut() = href.rsplit('/').next().map(str::to_string);
                }
            }
            Ok(())
        }),
        element!("input[name^=\"ZobrazeneVysledky\"][name$=\"ID\"]", |el| {
            if let Some(val) = el.get_attribute("value") {
                *doc_id.borrow_mut() = Some(val);
            }
            Ok(())
        }),
    ]);

    doc_id.into_inner()
}

#[derive(Default)]
struct NssMetadata {
    ecli: String,
    datum: String,
    soud_senat: String,
    pravni_veta: String,
}

fn extract_nss_metadata(detail_html: &str) -> NssMetadata {
    let meta = RefCell::new(NssMetadata::default());
    let current_label = RefCell::new(String::new());

    // Each field is a <div data-field-id="..."> whose value sits in span.det-textval
    let _ = rewrite(detail_html, vec![
        element!("div[data-field-id]", |el| {
            *current_label.borrow_mut() = el.get_attribute("data-field-id").unwrap_or_default();
            Ok(())
        }),
        text!("span.det-textval", |t| {
            let label = current_label.borrow().to_lowercase();
            let mut meta = meta.borrow_mut();
            let content = t.as_str();

            match label.as_str() {
                "ecli" => meta.ecli.push_str(content),
                // Only use datumvydanirozhodnuti to avoid duplicates from zobrazovanedatum
                "datumvydanirozhodnuti" => meta.datum.push_str(content),
                "soudsenat" => meta.soud_senat.push_str(content),
                // Only capture the "(text)" version, not the plain "Ano" version
                "pravnivetaupravena" => meta.pravni_veta.push_str(content),
                _ => {}
            }
            Ok(())
        }),
    ]);

    let meta = meta.into_inner();
    NssMetadata {
        ecli: decode(&meta.ecli),
        datum: decode(&meta.datum),
        soud_senat: decode(&meta.soud_senat),
        pravni_veta: decode(&meta.pravni_veta),
    }
}

fn clean_html_text(raw_html: &str) -> String {
    let preprocessed = raw_html
        .replace("<br>", "\n")
        .replace("<BR>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");

    let document = Html::parse_document(&preprocessed);
    let root = document
        .select(&Selector::parse("body").unwrap())
        .next()
        .unwrap_or_else(|| document.root_element());

    let mut text = String::new();
    extract_text_recursive(root, &mut text);

    text.trim().to_string()
}

fn extract_text_recursive(element: scraper::ElementRef, output: &mut String) {
    for node in element.children() {
        match node.value() {
            Node::Text(text_node) => output.push_str(&text_node.text),
            Node::Element(elem) => {
                let tag = elem.name().to_lowercase();
                if matches!(tag.as_str(), "script" | "style" | "noscript" | "head") {
                    continue;
                }
                if let Some(child_ref) = scraper::ElementRef::wrap(node) {
                    extract_text_recursive(child_ref, output);
                }
            }
            _ => {}
        }
    }
}

use reqwest::Client;
use std::sync::{Arc, Mutex};
use lol_html::{element, text, HtmlRewriter, Settings};
use crate::models::CaseResult;
use tracing::{info, warn};

pub async fn scrape_nejvyssi(
    client: &Client,
    search_query: &str,
) -> Result<CaseResult, Box<dyn std::error::Error + Send + Sync>> {
    info!("Scraping Supreme Court (NS) for: {}", search_query);

    // 1. Parse search query into components
    // Example: "3 Tdo 706/2024" -> senat=3, druh=Tdo, cislo=706, rok=2024
    let parts: Vec<&str> = search_query.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(format!("Invalid search query format: {}", search_query).into());
    }

    let senat = parts[0];
    let druh = parts[1];
    let rest = parts[2]; // "706/2024"
    let sub_parts: Vec<&str> = rest.split('/').collect();
    if sub_parts.len() < 2 {
        return Err(format!("Invalid case number format in: {}", rest).into());
    }
    let cislo = sub_parts[0];
    let rok = sub_parts[1];

    // 2. Construct Search URL
    // Refined URL found by subagent: WebSearch instead of $$WebSearch1
    let query = format!("[spzn1]={} AND [spzn2]={} AND [spzn3]={} AND [spzn4]={}", senat, druh, cislo, rok);
    let search_url = format!(
        "https://rozhodnuti.nsoud.cz/judikatura/judikatura_ns.nsf/WebSearch?SearchView&Query={}&SearchMax=1000&SearchOrder=4&Start=0&Count=20&pohled=1",
        urlencoding::encode(&query)
    );

    info!("Searching at: {}", search_url);

    // 3. Fetch search results
    let response = client.get(&search_url).send().await?;
    let status = response.status();
    let results_html = response.text().await?;

    // 4. Find detail link
    let detail_link = find_detail_link(&results_html);

    if let Some(href) = detail_link {
        let full_detail_url = if href.starts_with('/') {
            format!("https://rozhodnuti.nsoud.cz{}", href)
        } else {
            format!("https://rozhodnuti.nsoud.cz/judikatura/judikatura_ns.nsf/{}", href)
        };
        info!("Found detail URL: {}", full_detail_url);

        // Fetch detail page
        let detail_html = client.get(&full_detail_url).send().await?.text().await?;

        // Extract metadata from detail page
        let metadata = extract_ns_metadata(&detail_html);

        Ok(CaseResult {
            soud: vec!["nejvyssi".to_string()],
            spisova_znacka: search_query.to_string(),
            ecli: metadata.ecli,
            datum_rozhodnuti: metadata.datum,
            popularni_nazev: None, // Requested to be null
            url_adresa: full_detail_url,
            abstrakt: None, // Always null as requested
            pravni_veta: metadata.pravni_veta,
            kategorie: Some(metadata.kategorie),
            text_dokumentu: metadata.text_dokumentu,
        })
    } else {
        warn!("Case not found: {}", search_query);
        Err(format!("Case {} not found in NS database. Status: {}", search_query, status).into())
    }
}


fn find_detail_link(html: &str) -> Option<String> {
    let link = Arc::new(Mutex::new(None::<String>));
    
    let link_c1 = link.clone();
    let link_c2 = link.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("a.odk", move |el| {
                    if let Some(href) = el.get_attribute("href") {
                         let mut guard = link_c1.lock().unwrap();
                         if guard.is_none() {
                             *guard = Some(href);
                         }
                    }
                    Ok(())
                }),
                // Fallback for any link with OpenDocument if a.odk doesn't work
                element!("a", move |el| {
                    if let Some(href) = el.get_attribute("href") {
                        if href.contains("OpenDocument") && !href.contains(".pdf") && !href.contains(".rtf") {
                             let mut guard = link_c2.lock().unwrap();
                             if guard.is_none() {
                                 *guard = Some(href);
                             }
                        }
                    }
                    Ok(())
                })
            ],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );

    let _ = rewriter.write(html.as_bytes());
    let _ = rewriter.end();

    let res = link.lock().unwrap().clone();
    res
}

#[derive(Default, Clone)]
struct NsMetadata {
    ecli: String,
    datum: String,
    heslo: String,
    pravni_veta: String,
    kategorie: String,
    text_dokumentu: String,
}

fn extract_ns_metadata(html: &str) -> NsMetadata {
    let metadata = Arc::new(Mutex::new(NsMetadata::default()));
    let current_label = Arc::new(Mutex::new(String::new()));
    
    let label_c1 = current_label.clone();
    let label_c2 = current_label.clone();
    let label_c3 = current_label.clone();

    let metadata_c1 = metadata.clone();
    let text_content = Arc::new(Mutex::new(String::new()));
    let text_c = text_content.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                // Label extraction from td.left-part
                element!("td.left-part", move |_el| {
                    let mut label_guard = label_c1.lock().unwrap();
                    label_guard.clear();
                    Ok(())
                }),
                text!("td.left-part", move |t| {
                    let mut label_guard = label_c2.lock().unwrap();
                    label_guard.push_str(t.as_str());
                    Ok(())
                }),
                // Value extraction from td.right-part based on the preceding label
                text!("td.right-part", move |t| {
                    let label = label_c3.lock().unwrap().to_lowercase();
                    let mut meta = metadata_c1.lock().unwrap();
                    
                    if label.contains("ecli") {
                        meta.ecli.push_str(t.as_str());
                    } else if label.contains("datum rozhodnutí") {
                        meta.datum.push_str(t.as_str());
                    } else if label.contains("heslo") {
                        meta.heslo.push_str(t.as_str());
                    } else if label.contains("právní věta") {
                        meta.pravni_veta.push_str(t.as_str());
                    } else if label.contains("kategorie") {
                        meta.kategorie.push_str(t.as_str());
                    }
                    Ok(())
                }),
                // Main text extraction from the div with justify style
                text!("div[style*=\"text-align:justify\"]", move |t| {
                    text_c.lock().unwrap().push_str(t.as_str());
                    Ok(())
                })
            ],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );

    let _ = rewriter.write(html.as_bytes());
    let _ = rewriter.end();

    let mut final_meta = metadata.lock().unwrap().clone();
    
    // Decode HTML entities and trim
    final_meta.text_dokumentu = html_escape::decode_html_entities(text_content.lock().unwrap().trim()).into_owned();
    final_meta.ecli = html_escape::decode_html_entities(final_meta.ecli.trim()).into_owned();
    final_meta.datum = html_escape::decode_html_entities(final_meta.datum.trim()).into_owned();
    final_meta.heslo = html_escape::decode_html_entities(final_meta.heslo.trim()).into_owned();
    final_meta.pravni_veta = html_escape::decode_html_entities(final_meta.pravni_veta.trim()).into_owned();
    final_meta.kategorie = html_escape::decode_html_entities(final_meta.kategorie.trim()).into_owned();

    final_meta
}

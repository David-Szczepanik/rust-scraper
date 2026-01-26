use reqwest::Client;
use std::sync::{Arc, Mutex};
use lol_html::{element, text, HtmlRewriter, Settings};
use scraper::{Html, Selector, Node};
use crate::models::CaseResult;
use tracing::{info, warn};

async fn extract_nss_form_fields(html: &str) -> std::collections::HashMap<String, String> {
    let fields = Arc::new(Mutex::new(std::collections::HashMap::<String, String>::new()));
    let fields_c = fields.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("input", move |el| {
                    let name = el.get_attribute("name");
                    let value = el.get_attribute("value").unwrap_or_default();
                    if let Some(n) = name {
                        fields_c.lock().unwrap().insert(n, value);
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

    let res = fields.lock().unwrap().clone();
    res
}

pub async fn scrape_nejvyssi_spravni(
    client: &Client,
    search_query: &str,
) -> Result<CaseResult, Box<dyn std::error::Error + Send + Sync>> {
    info!("Scraping Supreme Administrative Court (NSS) for: {}", search_query);

    // 1. Get the search page to capture all hidden form fields (including __RequestVerificationToken)
    let search_page_url = "https://vyhledavac.nssoud.cz/";
    let search_page_html = client.get(search_page_url).send().await?.text().await?;
    
    let mut form_data = extract_nss_form_fields(&search_page_html).await;
    
    // 2. Prepare search parameters
    // Using the field name for full case number 'Označení věci v celku'
    form_data.insert(
        "vyhledavaciSekce[0].vyhledavaciPodminka[1].vyhledavaciPodminkaHodnota[0].HodnotaText".to_string(),
        search_query.to_string()
    );
    
    // Ensure "Formular" is "1"
    form_data.insert("Formular".to_string(), "1".to_string());

    // 3. Perform POST search
    let response = client.post("https://vyhledavac.nssoud.cz/")
        .form(&form_data)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("NSS search failed with status: {}", response.status()).into());
    }

    let results_html = response.text().await?;

    // 3. Find document ID from results
    let doc_id = find_nss_doc_id(&results_html);

    if let Some(id) = doc_id {
        info!("Found NSS document ID: {}", id);
        
        // Fetch Text version of the document directly (cleaner for text)
        let text_url = format!("https://vyhledavac.nssoud.cz/DokumentOriginal/Text/{}", id);
        let detail_url = format!("https://vyhledavac.nssoud.cz/DokumentDetail/Index/{}", id);
        
        let doc_text = client.get(&text_url).send().await?.text().await?;
        let detail_html = client.get(&detail_url).send().await?.text().await?;

        // Extract metadata from detail page
        let metadata = extract_nss_metadata(&detail_html);

        // Parse HTML to extract plain text with custom cleaning
        let clean_text = clean_html_text(&doc_text);

        Ok(CaseResult {
            soud: vec!["nejvyssi_spravni".to_string(), metadata.soud_senat],
            spisova_znacka: search_query.to_string(),
            ecli: metadata.ecli,
            datum_rozhodnuti: metadata.datum,
            popularni_nazev: None,
            url_adresa: detail_url,
            abstrakt: None,
            pravni_veta: metadata.pravni_veta,
            kategorie: None,
            text_dokumentu: clean_text,
        })
    } else {
        warn!("NSS case not found: {}", search_query);
        Err(format!("Case {} not found in NSS database", search_query).into())
    }
}



fn find_nss_doc_id(html: &str) -> Option<String> {
    let doc_id = Arc::new(Mutex::new(None::<String>));
    let doc_id_c1 = doc_id.clone();
    let doc_id_c2 = doc_id.clone();

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                // Look for links like /DokumentOriginal/Html/724188
                element!("a", move |el| {
                    if let Some(href) = el.get_attribute("href") {
                        if href.contains("/DokumentOriginal/Html/") {
                            let parts: Vec<&str> = href.split('/').collect();
                            if let Some(id) = parts.last() {
                                *doc_id_c1.lock().unwrap() = Some(id.to_string());
                            }
                        }
                    }
                    Ok(())
                }),
                // Or hidden input: <input name="ZobrazeneVysledky[0].ID" type="hidden" value="724188">
                element!("input[name^=\"ZobrazeneVysledky\"][name$=\"ID\"]", move |el| {
                    if let Some(val) = el.get_attribute("value") {
                        *doc_id_c2.lock().unwrap() = Some(val);
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

    let res = doc_id.lock().unwrap().clone();
    res
}

#[derive(Default, Clone)]
struct NssMetadata {
    ecli: String,
    datum: String,
    soud_senat: String,
    heslo: String,
    pravni_veta: String,
}

fn extract_nss_metadata(detail_html: &str) -> NssMetadata {
    let metadata = Arc::new(Mutex::new(NssMetadata::default()));
    let current_label = Arc::new(Mutex::new(String::new()));
    
    let label_c1 = current_label.clone();

    let label_c3 = current_label.clone();

    let metadata_c1 = metadata.clone();

    // NSS detail page structure:
    // <div class="label">Datum rozhodnutí</div>
    // <div class="value">22.08.2024</div>
    // Note: This is an assumption, let's use the subagent's findings if they were more specific.
    // Actually, subagent just said "structured table format".
    
    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("div[data-field-id]", move |el| {
                    let mut label_guard = label_c1.lock().unwrap();
                    if let Some(id) = el.get_attribute("data-field-id") {
                        *label_guard = id;
                    } else {
                        label_guard.clear();
                    }
                    Ok(())
                }),
                text!("span.det-textval", move |t| {
                    let label = label_c3.lock().unwrap().to_lowercase();
                    let mut meta = metadata_c1.lock().unwrap();
                    
                    let content = t.as_str();
                    
                    if label == "ecli" {
                        meta.ecli.push_str(content);
                    } else if label == "datumvydanirozhodnuti" {
                        // Only use datumvydanirozhodnuti to avoid duplicates from zobrazovanedatum
                        meta.datum.push_str(content);
                    } else if label == "soudsenat" {
                        meta.soud_senat.push_str(content);
                    } else if label == "oblastupravy" {
                        if !meta.heslo.is_empty() && !content.trim().is_empty() {
                            meta.heslo.push_str(", ");
                        }
                        meta.heslo.push_str(content);
                    } else if label == "pravnivetaupravena" || label == "pravnivetaan" {
                        meta.pravni_veta.push_str(content);
                    }
                    Ok(())
                }),
            ],
            ..Settings::default()
        },
        |_: &[u8]| {},
    );

    let _ = rewriter.write(detail_html.as_bytes());
    let _ = rewriter.end();

    let mut final_meta = metadata.lock().unwrap().clone();
    
    // Decode HTML entities and trim
    final_meta.ecli = html_escape::decode_html_entities(final_meta.ecli.trim()).into_owned();
    final_meta.datum = html_escape::decode_html_entities(final_meta.datum.trim()).into_owned();
    final_meta.soud_senat = html_escape::decode_html_entities(final_meta.soud_senat.trim()).into_owned();
    final_meta.heslo = html_escape::decode_html_entities(final_meta.heslo.trim()).into_owned();
    final_meta.pravni_veta = html_escape::decode_html_entities(final_meta.pravni_veta.trim()).into_owned();

    final_meta
}

fn clean_html_text(raw_html: &str) -> String {
    // 1. Pre-process to turn <br> into newlines before parsing
    // This handles <br>, <br/>, <br /> cases.
    let preprocessed = raw_html
        .replace("<br>", "\n")
        .replace("<BR>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");

    // 2. Parse fragment
    let document = Html::parse_document(&preprocessed);
    
    // 3. Select body (or fallback to root if malformed)
    // We try to find the body element to start traversal.
    let root = if let Some(body) = document.select(&Selector::parse("body").unwrap()).next() {
        body
    } else {
        document.root_element()
    };

    // 4. Traverse and extract text, skipping script/style
    let mut text = String::new();
    extract_text_recursive(root, &mut text);
    
    text.trim().to_string()
}

fn extract_text_recursive(element: scraper::ElementRef, output: &mut String) {
    for node in element.children() {
        match node.value() {
            Node::Text(text_node) => {
                output.push_str(&text_node.text);
            },
            Node::Element(elem) => {
                // Check tag name
                let tag = elem.name().to_lowercase();
                if tag == "script" || tag == "style" || tag == "noscript" || tag == "head" {
                    continue;
                }
                
                if let Some(child_ref) = scraper::ElementRef::wrap(node) {
                    extract_text_recursive(child_ref, output);
                }
            },
            _ => {} // Ignore comments, doctypes, etc.
        }
    }
}

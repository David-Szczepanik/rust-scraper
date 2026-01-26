use reqwest::Client;
use lol_html::{element, text, HtmlRewriter, Settings};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use url::Url;
use tracing::{debug, info, warn};
use crate::models::CaseResult;

const BASE_URL: &str = "https://nalus.usoud.cz/Search";

pub async fn scrape_ustavni(
    client: &Client,
    search_query: &str,
) -> Result<CaseResult, Box<dyn std::error::Error + Send + Sync>> {
    let search_url = format!("{}/Search.aspx", BASE_URL);

    // 1. fetch search page
    let initial_html = client.get(&search_url).send().await?.text().await?;
    let mut form_data = extract_hidden_fields(&initial_html)?;

    // 2. submit search
    info!("Submitting search for '{}' with {} form fields", search_query, form_data.len());
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
    let (target_href, found_citation) = find_result_href(&result_page_html, search_query).ok_or_else(|| {
        let msg = format!("Link not found for {}. Status: {}", search_query, status);
        Box::<dyn std::error::Error + Send + Sync>::from(msg)
    })?;


    let detail_url = format!("{}/{}", BASE_URL, target_href);

    // 4. fetch detail page
    let detail_html = client.get(&detail_url).send().await?.text().await?;
    let (ecli, datum, popularni_nazev) = extract_metadata(&detail_html);
    let text_dokumentu = extract_document_text(&detail_html);

    // 5. fetch Abstract separately if "ShowAbstrakt" button exists
    let (mut abstrakt, mut pravni_veta) = if has_show_abstrakt(&detail_html) {
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

    // Decode HTML entities for all strings
    let ecli = html_escape::decode_html_entities(ecli.trim()).into_owned();
    let datum = html_escape::decode_html_entities(datum.trim()).into_owned();
    let mut popularni_nazev = html_escape::decode_html_entities(popularni_nazev.trim()).into_owned();
    let text_dokumentu = html_escape::decode_html_entities(text_dokumentu.trim()).into_owned();
    abstrakt = html_escape::decode_html_entities(abstrakt.trim()).into_owned();
    pravni_veta = html_escape::decode_html_entities(pravni_veta.trim()).into_owned();

    if popularni_nazev.trim().is_empty() {
        popularni_nazev = String::new();
    }

    Ok(CaseResult {
        soud: vec!["ustavni".to_string()],
        spisova_znacka: standardize_spisova_znacka(&found_citation),
        ecli,
        datum_rozhodnuti: datum,
        popularni_nazev: Some(popularni_nazev),
        url_adresa: detail_url,
        abstrakt: Some(abstrakt),
        pravni_veta,
        kategorie: None,
        text_dokumentu,
    })
}

fn standardize_spisova_znacka(input: &str) -> String {
    let mut result = String::with_capacity(input.len() + 5);
    let chars: Vec<char> = input.chars().collect();

    for (i, &c) in chars.iter().enumerate() {
        result.push(c);
        if c == '.' {
            // Check if followed by non-whitespace
            if i + 1 < chars.len() && !chars[i + 1].is_whitespace() {
                // Check prefix (word before dot)
                let mut start = i;
                while start > 0 && !chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                let prefix: String = chars[start..i].iter().collect();

                let is_roman = !prefix.is_empty()
                    && prefix
                        .chars()
                        .all(|c| matches!(c, 'I' | 'V' | 'X' | 'L' | 'C' | 'D' | 'M'));
                let is_pl = prefix == "Pl";

                if is_roman || is_pl {
                    result.push(' ');
                }
            }
        }
    }
    result
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

fn find_result_href(html: &str, query: &str) -> Option<(String, String)> {
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

    info!("Found {} link candidates", candidates.len());
    for (i, (href, text)) in candidates.iter().enumerate() {
        debug!("Candidate {}: text='{}', href='{}'", i, text, href);
    }

    let normalize_base = |s: &str| -> String {
        s.to_lowercase()
            .replace("ú", "u")
            .replace("ů", "u")
            .replace(".", "")
    };

    let strip_whitespace = |s: &str| -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    };

    let normalize_strict = |s: &str| -> String {
        strip_whitespace(&normalize_base(s))
    };

    let normalize_loose = |s: &str| -> String {
        normalize_base(s).replace(" ", "")
    };

    let query_strict = normalize_strict(query);
    let query_loose = normalize_loose(query);

    info!(
        "Looking for query: '{}'. Strict: '{}', Loose: '{}'",
        query, query_strict, query_loose
    );

    if candidates.is_empty() {
        warn!("No candidates found with 'a.resultData0' selector. HTML length: {} bytes", html.len());
        if html.len() > 500 {
            debug!("HTML snippet: {}", &html[..500]);
        }
    }

    for (href, text) in candidates.iter() {
        let text_strict = normalize_strict(text);
        debug!("Checking candidate text: '{}' -> normalized: '{}'", text, text_strict);

        if text_strict.starts_with(&query_strict) {
            let remainder = &text_strict[query_strict.len()..];

            if remainder.is_empty() || !remainder.chars().next().unwrap().is_alphanumeric() {
                info!("Found strict match for '{}': '{}'", query, text);
                return Some((href.clone(), text.clone()));
            }
        }
    }

    for (href, text) in candidates.iter() {
        let text_loose = normalize_loose(text);
        debug!("Checking candidate (loose) text: '{}' -> normalized: '{}'", text, text_loose);

        if !query_loose.is_empty() && text_loose.starts_with(&query_loose) {
             let remainder = &text_loose[query_loose.len()..];
             if remainder.is_empty() || !remainder.chars().next().unwrap().is_alphanumeric() {
                 info!("Found loose match for '{}': '{}'", query, text);
                 return Some((href.clone(), text.clone()));
             }
        }
    }

    warn!("Match not found for '{}' among {} candidates", query, candidates.len());
    for (i, (_href, text)) in candidates.iter().enumerate() {
        info!("  Candidate {}: '{}' (Strict: {}, Loose: {})", i, text, normalize_strict(text), normalize_loose(text));
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
                    abstract_text_br.lock().unwrap().push_str("\n");
                    Ok(())
                }),
                element!(".legalSentenceContent br", move |_| {
                    legal_text_br.lock().unwrap().push_str("\n");
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

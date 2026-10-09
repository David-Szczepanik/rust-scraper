use reqwest::Client;
use lol_html::{element, text};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use url::Url;
use tracing::{debug, info, warn};
use crate::models::CaseResult;
use super::common::{decode, fetch_text, rewrite, BoxError};

const BASE_URL: &str = "https://nalus.usoud.cz/Search";

/// (href, citation text) pairs from a NALUS result list.
type Citations = Vec<(String, String)>;

pub async fn scrape_ustavni(
    client: &Client,
    search_query: &str,
) -> Result<CaseResult, BoxError> {
    let search_url = format!("{}/Search.aspx", BASE_URL);

    let form_data = extract_hidden_fields(&fetch_text(client, &search_url).await?)?;

    let mut status_code = reqwest::StatusCode::OK;
    let mut found = None;

    // Try year variants (e.g. 10 and 2010)
    for query in crate::models::get_year_variants(search_query) {
        info!("Submitting search for '{}' with {} form fields", query, form_data.len());
        let form = search_form(&form_data, &[("ctl00$MainContent$citace", &query)]);

        let response = client.post(&search_url).form(&form).send().await?;
        status_code = response.status();
        let result_page_html = response.text().await?;
        info!("Search request for '{}' returned status: {}, content length: {} bytes", query, status_code, result_page_html.len());

        found = find_result_href(&result_page_html, &query);
        if found.is_some() {
            break;
        }
    }

    match found {
        Some((href, citation)) if !href.is_empty() => fetch_case_detail(client, &href, &citation).await,
        _ => Err(format!("Link not found for variants of {}. Last status: {}", search_query, status_code).into()),
    }
}

/// Copies the hidden ASP.NET fields and adds the options every NALUS search sends.
fn search_form(base: &HashMap<String, String>, extra: &[(&str, &str)]) -> HashMap<String, String> {
    let defaults = [
        ("ctl00$MainContent$nalezy", "on"),
        ("ctl00$MainContent$usneseni", "on"),
        ("ctl00$MainContent$stanoviska_plena", "on"),
        ("ctl00$MainContent$but_search", "Vyhledat"),
    ];
    let mut form = base.clone();
    for (key, value) in defaults.iter().chain(extra) {
        form.insert(key.to_string(), value.to_string());
    }
    form
}

/// Adds the missing space after a senate prefix: "IV.ÚS 1/1" -> "IV. ÚS 1/1", "Pl.ÚS 19/08" -> "Pl. ÚS 19/08".
fn standardize_spisova_znacka(input: &str) -> String {
    let mut result = String::with_capacity(input.len() + 5);
    let chars: Vec<char> = input.chars().collect();

    for (i, &c) in chars.iter().enumerate() {
        result.push(c);
        if c == '.' {
            if i + 1 < chars.len() && !chars[i + 1].is_whitespace() {
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

/// Text inside `selector`, with each `<br>` turned into a newline.
fn text_with_breaks(html: &str, selector: &str) -> String {
    let out = RefCell::new(String::new());
    let _ = rewrite(html, vec![
        text!(selector, |t| {
            out.borrow_mut().push_str(t.as_str());
            Ok(())
        }),
        element!(format!("{} br", selector), |_| {
            out.borrow_mut().push('\n');
            Ok(())
        }),
    ]);
    out.into_inner().trim().to_string()
}

fn extract_hidden_fields(html: &str) -> Result<HashMap<String, String>, BoxError> {
    let target_fields = [
        "__VIEWSTATE",
        "__EVENTVALIDATION",
        "__VIEWSTATEGENERATOR",
        "__PREVIOUSPAGE",
    ];
    let values = RefCell::new(HashMap::new());

    rewrite(html, vec![element!("input[type=hidden]", |el| {
        if let Some(name) = el.get_attribute("name") {
            if target_fields.contains(&name.as_str()) {
                let value = el.get_attribute("value").unwrap_or_default();
                values.borrow_mut().insert(name, value);
            }
        }
        Ok(())
    })])?;

    Ok(values.into_inner())
}

fn find_all_result_hrefs(html: &str) -> Citations {
    let candidates = RefCell::new(Citations::new());
    let _ = rewrite(html, vec![
        element!("a.resultData0, a.resultData1", |el| {
            let href = el.get_attribute("href").unwrap_or_default();
            candidates.borrow_mut().push((href, String::new()));
            Ok(())
        }),
        text!("a.resultData0, a.resultData1", |t| {
            if let Some(last) = candidates.borrow_mut().last_mut() {
                last.1.push_str(t.as_str());
            }
            Ok(())
        }),
    ]);
    candidates.into_inner()
}

fn find_result_href(html: &str, query: &str) -> Option<(String, String)> {
    let candidates = find_all_result_hrefs(html);

    info!("Found {} link candidates", candidates.len());
    for (i, (href, text)) in candidates.iter().enumerate() {
        debug!("Candidate {}: text='{}', href='{}'", i, text, href);
    }

    let normalize_base = |s: &str| -> String {
        let base = s.to_lowercase()
            .replace("ú", "u")
            .replace("ů", "u")
            .replace(".", "");

        crate::models::shorten_year(&base)
    };

    let normalize_strict = |s: &str| -> String {
        normalize_base(s).chars().filter(|c| !c.is_whitespace()).collect()
    };

    let normalize_loose = |s: &str| -> String {
        normalize_base(s).replace(" ", "")
    };

    // `text` starts with `query` and the match does not end mid-word.
    let is_prefix_match = |text: &str, query: &str| -> bool {
        text.starts_with(query)
            && text[query.len()..].chars().next().map_or(true, |c| !c.is_alphanumeric())
    };

    let query_strict = normalize_strict(query);
    let query_loose = normalize_loose(query);

    info!(
        "Looking for query: '{}'. Strict: '{}', Loose: '{}'",
        query, query_strict, query_loose
    );

    if candidates.is_empty() {
        warn!("No candidates found with resultData selectors. HTML length: {} bytes", html.len());
        if html.len() > 500 {
            debug!("HTML snippet: {}", &html[..500]);
        }
    }

    for (href, text) in candidates.iter() {
        let text_strict = normalize_strict(text);
        debug!("Checking candidate text: '{}' -> normalized: '{}'", text, text_strict);

        if is_prefix_match(&text_strict, &query_strict) {
            info!("Found strict match for '{}': '{}'", query, text);
            return Some((href.clone(), text.clone()));
        }
    }

    for (href, text) in candidates.iter() {
        let text_loose = normalize_loose(text);
        debug!("Checking candidate (loose) text: '{}' -> normalized: '{}'", text, text_loose);

        if !query_loose.is_empty() && is_prefix_match(&text_loose, &query_loose) {
            info!("Found loose match for '{}': '{}'", query, text);
            return Some((href.clone(), text.clone()));
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

/// Returns (ecli, datum, popularni_nazev) from the record card table.
fn extract_metadata(html: &str) -> (String, String, String) {
    // A label cell sets `next`; the following cell becomes `current` and holds the value.
    let next = Cell::new(None::<MetaField>);
    let current = Cell::new(None::<MetaField>);
    let ecli = RefCell::new(String::new());
    let datum = RefCell::new(String::new());
    let popular_title = RefCell::new(String::new());

    let _ = rewrite(html, vec![
        element!("table.recordCardTable td", |_| {
            current.set(next.take());
            Ok(())
        }),
        text!("table.recordCardTable td", |t| {
            let text = t.as_str();
            match current.get() {
                Some(MetaField::Ecli) => ecli.borrow_mut().push_str(text),
                Some(MetaField::Datum) => datum.borrow_mut().push_str(text),
                Some(MetaField::PopularTitle) => popular_title.borrow_mut().push_str(text),
                None => {}
            }
            if text.contains("Identifikátor evropské judikatury") {
                next.set(Some(MetaField::Ecli));
            } else if text.contains("Datum rozhodnutí") {
                next.set(Some(MetaField::Datum));
            } else if text.contains("Populární název") {
                next.set(Some(MetaField::PopularTitle));
            }
            Ok(())
        }),
    ]);

    let clean_datum = datum.into_inner().trim().replace("Forma rozhodnutí", "").trim().to_string();

    (
        ecli.into_inner().trim().to_string(),
        clean_datum,
        popular_title.into_inner().trim().to_string(),
    )
}

fn has_show_abstrakt(html: &str) -> bool {
    let found = Cell::new(false);
    let _ = rewrite(html, vec![element!("input[name='ShowAbstrakt']", |_| {
        found.set(true);
        Ok(())
    })]);
    found.get()
}

/// Returns (abstrakt, pravni_veta); a placeholder "not available" text becomes empty.
fn extract_abstrakt_content(html: &str) -> (String, String) {
    let abstrakt = text_with_breaks(html, ".abstractContent");
    let pravni_veta = text_with_breaks(html, ".legalSentenceContent");

    let final_abs = if abstrakt.contains("Abstrakt není k dispozici") {
        String::new()
    } else {
        abstrakt
    };
    let final_legal = if pravni_veta.contains("Právní věta není k dispozici") {
        String::new()
    } else {
        pravni_veta
    };

    (final_abs, final_legal)
}

pub async fn fetch_case_detail(
    client: &Client,
    href: &str,
    citation: &str,
) -> Result<CaseResult, BoxError> {
    let detail_url = format!("{}/{}", BASE_URL, href);

    let detail_html = fetch_text(client, &detail_url).await?;
    let (ecli, datum, popularni_nazev) = extract_metadata(&detail_html);
    let text_dokumentu = text_with_breaks(&detail_html, "#uc_vytah_cellContent");

    // The abstract and legal sentence live on a separate page
    let (abstrakt, pravni_veta) = if has_show_abstrakt(&detail_html) {
        let parsed_url = Url::parse(&detail_url)?;
        let id_param = parsed_url
            .query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.to_string())
            .ok_or("Could not find 'id' parameter in detail URL")?;

        let abstrakt_url = format!("{}/Abstrakt.aspx?id={}", BASE_URL, id_param);
        extract_abstrakt_content(&fetch_text(client, &abstrakt_url).await?)
    } else {
        (String::new(), String::new())
    };

    let popularni_nazev = decode(&popularni_nazev);
    let popularni_nazev = if popularni_nazev.trim().is_empty() {
        String::new()
    } else {
        popularni_nazev
    };

    Ok(CaseResult {
        soud: vec!["ustavni".to_string()],
        spisova_znacka: standardize_spisova_znacka(citation),
        ecli: decode(&ecli),
        datum_rozhodnuti: decode(&datum),
        popularni_nazev: Some(popularni_nazev),
        url_adresa: detail_url,
        abstrakt: Some(decode(&abstrakt)),
        pravni_veta: decode(&pravni_veta),
        kategorie: None,
        text_dokumentu: decode(&text_dokumentu),
        found_in_db: false,
    })
}

/// Search Ustavni court and return citations/hrefs without fetching details yet.
pub async fn search_ustavni_citations(
    client: &Client,
    phrases: &[String],
    keywords: &[String],
) -> Result<(HashMap<String, Citations>, HashMap<String, Citations>), BoxError> {
    let search_url = format!("{}/Search.aspx", BASE_URL);
    let mut keyword_citations = HashMap::new();
    let mut phrase_citations = HashMap::new();

    let base_form_data = extract_hidden_fields(&fetch_text(client, &search_url).await?)?;

    for keyword in keywords.iter().filter(|k| !k.trim().is_empty()) {
        let form = search_form(&base_form_data, &[
            ("ctl00$MainContent$popularni_nazev", keyword),
            ("ctl00$MainContent$resultsPageSize", "20"),
        ]);
        let result_html = client.post(&search_url).form(&form).send().await?.text().await?;
        let hrefs = find_all_result_hrefs(&result_html);
        info!("Keyword '{}' found {} citations", keyword, hrefs.len());
        keyword_citations.insert(keyword.clone(), hrefs);
    }

    for phrase in phrases.iter().filter(|p| !p.trim().is_empty()) {
        let form = search_form(&base_form_data, &[
            ("ctl00$MainContent$text", phrase),
            ("ctl00$MainContent$pravni_veta", "on"),
            ("ctl00$MainContent$abstrakt", "on"),
            ("ctl00$MainContent$resultsPageSize", "20"),
        ]);
        let result_html = client.post(&search_url).form(&form).send().await?.text().await?;
        let hrefs = find_all_result_hrefs(&result_html);
        info!("Phrase '{}' found {} citations", phrase, hrefs.len());
        phrase_citations.insert(phrase.clone(), hrefs);
    }

    Ok((keyword_citations, phrase_citations))
}

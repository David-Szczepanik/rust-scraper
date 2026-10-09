use std::borrow::Cow;
use lol_html::errors::RewritingError;
use lol_html::{ElementContentHandlers, HtmlRewriter, Selector, Settings};
use reqwest::Client;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub async fn fetch_text(client: &Client, url: &str) -> Result<String, BoxError> {
    Ok(client.get(url).send().await?.text().await?)
}

pub fn rewrite(
    html: &str,
    handlers: Vec<(Cow<'_, Selector>, ElementContentHandlers<'_>)>,
) -> Result<(), RewritingError> {
    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: handlers,
            ..Settings::new()
        },
        |_: &[u8]| {},
    );
    rewriter.write(html.as_bytes())?;
    rewriter.end()
}

pub fn decode(s: &str) -> String {
    html_escape::decode_html_entities(s.trim()).into_owned()
}

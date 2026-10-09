mod common;
pub use common::BoxError;
pub mod ustavni;
pub mod nejvyssi;
pub mod nejvyssi_spravni;

pub use ustavni::{scrape_ustavni, search_ustavni_citations, fetch_case_detail};
pub use nejvyssi::scrape_nejvyssi;
pub use nejvyssi_spravni::scrape_nejvyssi_spravni;

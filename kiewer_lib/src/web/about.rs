//! The about page: what kiewer is, and the credits the things it bundles
//! ask for. The same for everyone: nothing in it depends on the session.

use askama::Template;
use axum::response::Html;

use crate::error::Result;

#[derive(Template)]
#[template(path = "about.html")]
struct About {
    build: String,
}

/// `GET /about`.
pub async fn page() -> Result<Html<String>> {
    let page = About {
        build: crate::build::line(),
    };
    Ok(Html(page.render()?))
}

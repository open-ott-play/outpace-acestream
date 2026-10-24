//! Source offer for the exact source tree embedded when this binary was built.
use axum::body::Body;
use axum::http::{header, HeaderValue};
use axum::response::{Html, IntoResponse, Response};

pub const ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/corresponding-source.tar.gz"));
pub const METADATA: &str = include_str!(concat!(env!("OUT_DIR"), "/source.json"));
const LICENSE: &str = include_str!("../../../LICENSE");

pub async fn landing() -> Html<&'static str> {
    Html(concat!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">",
        "<meta name=\"viewport\" content=\"width=device-width\"><title>Outpace AceStream — source</title>",
        "<main><h1>Outpace AceStream</h1><p>Version ", env!("CARGO_PKG_VERSION"),
        "; independent development of the Outpace Rust AceStream engine.</p>",
        "<h2>Get the source for this running build</h2>",
        "<p>This program is free software under the <a href=\"/license\">GNU Affero General Public License, version 3 or later</a>.</p>",
        "<p><a href=\"/source.tar.gz\" download>Download the corresponding source (tar.gz)</a> free of charge. ",
        "The archive is embedded in this executable at build time and includes source, Cargo.lock, build scripts, license and notices.</p>",
        "<p><a href=\"/source.json\">Build identity and source SHA-256</a>. The Git revision identifies the base; ",
        "the source checksum identifies the actual packaged tree, including tracked modifications.</p>",
        "<p><a href=\"https://github.com/open-ott-play/outpace-acestream\">Project and rebuild instructions</a> · ",
        "<a href=\"https://github.com/jamezrin/outpace\">Original Outpace project</a></p>",
        "<p>Copyright belongs to the Outpace contributors and subsequent Outpace AceStream contributors. ",
        "Provided without warranty; see the license for details.</p></main></html>"
    ))
}

pub async fn archive() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/gzip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"outpace-acestream-source.tar.gz\"",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from(ARCHIVE),
    )
        .into_response()
}

pub async fn metadata() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        METADATA,
    )
        .into_response()
}

pub async fn license() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        LICENSE,
    )
        .into_response()
}

/// A zero-buffering response hook also advertises the offer to API clients.
pub async fn advertise(mut response: Response) -> Response {
    response.headers_mut().append(
        header::LINK,
        HeaderValue::from_static("</source>; rel=\"describedby\"; title=\"Corresponding source\", </license>; rel=\"license\""),
    );
    response
}

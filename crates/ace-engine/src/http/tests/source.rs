use super::*;
use sha2::{Digest, Sha256};

#[tokio::test]
async fn source_offer_is_available_without_compat_or_active_streams() {
    let app = router(state());
    for path in ["/", "/source"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("href=\"/source.tar.gz\""));
        assert!(html.contains("GNU Affero General Public License"));
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/source.tar.gz?path=/private/ignored")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/gzip");
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), crate::source::ARCHIVE);
    let metadata: serde_json::Value = serde_json::from_str(crate::source::METADATA).unwrap();
    assert_eq!(
        metadata["source_sha256"],
        format!("{:x}", Sha256::digest(&bytes))
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.headers()[header::LINK]
        .to_str()
        .unwrap()
        .contains("/source"));
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 32)
            .await
            .unwrap(),
        "ok"
    );
}

use super::*;
use crate::acestream_search::{SearchError, SearchQuery, SearchService};
use axum::extract::OriginalUri;

/// Search is mounted separately and remains independent of legacy playback compatibility.
pub(super) fn router(service: Option<Arc<SearchService>>) -> Router {
    match service {
        Some(service) => Router::new()
            .route("/search", get(search))
            .with_state(service),
        None => Router::new(),
    }
}

pub(super) async fn search(
    State(service): State<Arc<SearchService>>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let raw = uri.query().unwrap_or_default();
    let query = if raw.len() > 2048 {
        Err(SearchError::InvalidQuery)
    } else {
        SearchQuery::parse(
            form_urlencoded::parse(raw.as_bytes())
                .into_owned()
                .collect(),
        )
    };
    let result = match query {
        Ok(query) => service.search(query).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => {
            let (status, code) = match error {
                SearchError::InvalidQuery => (StatusCode::BAD_REQUEST, "search_invalid_query"),
                SearchError::InvalidResponse => {
                    (StatusCode::BAD_GATEWAY, "search_invalid_upstream_response")
                }
                SearchError::TooLarge => (StatusCode::BAD_GATEWAY, "search_response_too_large"),
                SearchError::Upstream => (StatusCode::BAD_GATEWAY, "search_upstream_unavailable"),
                SearchError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "search_timeout"),
                SearchError::Busy => (StatusCode::SERVICE_UNAVAILABLE, "search_busy"),
                SearchError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "search_rate_limited"),
                SearchError::InvalidSettings => {
                    (StatusCode::SERVICE_UNAVAILABLE, "search_unavailable")
                }
            };
            let mut response = (status, Json(json!({"result":null,"error":code}))).into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            if error == SearchError::RateLimited {
                response.headers_mut().insert(
                    header::RETRY_AFTER,
                    axum::http::HeaderValue::from_static("30"),
                );
            }
            if error == SearchError::Busy {
                response.headers_mut().insert(
                    header::RETRY_AFTER,
                    axum::http::HeaderValue::from_static("1"),
                );
            }
            response
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acestream_search::offline_service;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn request(app: Router, path: &str) -> Response {
        app.oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn disabled_search_has_no_route_and_enabled_search_returns_normalized_json() {
        assert_eq!(
            request(router(None), "/search").await.status(),
            StatusCode::NOT_FOUND
        );
        let service = offline_service(Ok(br#"{"total":0,"time":0.0,"results":[]}"#.to_vec()));
        let response = request(router(Some(service)), "/search").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            json!({"result":{"total":0,"results":[]},"error":null})
        );
    }

    #[tokio::test]
    async fn malformed_queries_do_not_consume_the_offline_response() {
        let app = router(Some(offline_service(Ok(
            br#"{"total":0,"time":0.0,"results":[]}"#.to_vec(),
        ))));
        for path in [
            "/search?page_size=0",
            "/search?page=-1",
            "/search?query=a&query=b",
            "/search?url=http://127.0.0.1/",
        ] {
            assert_eq!(
                request(app.clone(), path).await.status(),
                StatusCode::BAD_REQUEST
            );
        }
        let oversized = format!("/search?{}", "x=".repeat(2048));
        assert_eq!(
            request(app.clone(), &oversized).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(request(app, "/search").await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn upstream_failure_is_not_an_empty_search_and_enters_cooldown() {
        let app = router(Some(offline_service(Err(SearchError::Upstream))));
        let response = request(app.clone(), "/search").await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["result"].is_null());
        assert_eq!(body["error"], "search_upstream_unavailable");
        let response = request(app, "/search").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "30");
    }
}

use super::*;
use axum::http::HeaderMap;

#[test]
fn restricted_traffic_requires_its_token() {
    let mut headers = HeaderMap::new();
    assert_eq!(
        check_traffic_access(Some("secret"), &headers)
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    headers.insert(TRAFFIC_TOKEN_HEADER, "wrong".parse().unwrap());
    assert_eq!(
        check_traffic_access(Some("secret"), &headers)
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    headers.insert(TRAFFIC_TOKEN_HEADER, "secret".parse().unwrap());
    check_traffic_access(Some("secret"), &headers).unwrap();
}

#[test]
fn public_traffic_needs_no_token() {
    check_traffic_access(None, &HeaderMap::new()).unwrap();
}

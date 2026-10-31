use axum::body::Body;
use axum::http::{Request, StatusCode};
use sandstorm_core::identity::Ca;
use sandstorm_core::master::e2b::{self, E2b};
use sandstorm_core::master::{MasterOptions, MasterState};
use sandstorm_core::state::Store;
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

#[tokio::test]
async fn e2b_create_options_are_checked_before_creating_a_sandbox() {
    let directory = TempDir::new().unwrap();
    let state = MasterState::new(
        Store::open(directory.path().join("state.db")).unwrap(),
        Ca::open_or_create(directory.path().join("pki")).unwrap(),
        MasterOptions {
            api_token: Some("test-token".into()),
            ..Default::default()
        },
    );
    let app = e2b::router(E2b::new(state.into(), "e2b.test"));

    for (option, expected_status) in [
        (json!({"secure": false}), StatusCode::NOT_IMPLEMENTED),
        (
            json!({"allow_internet_access": true}),
            StatusCode::NOT_FOUND,
        ),
        (json!({"allowInternetAccess": true}), StatusCode::NOT_FOUND),
        (
            json!({"allow_internet_access": false}),
            StatusCode::NOT_FOUND,
        ),
        (json!({"allowInternetAccess": false}), StatusCode::NOT_FOUND),
        (
            json!({"network": {"allowPublicTraffic": false}}),
            StatusCode::NOT_FOUND,
        ),
        (
            json!({"network": {"allowPublicTraffic": false, "allowOut": ["8.8.8.8"]}}),
            StatusCode::NOT_FOUND,
        ),
        (
            json!({"network": {"allowPublicTraffic": "no"}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let mut request_body = json!({"templateID": "base"});
        request_body
            .as_object_mut()
            .unwrap()
            .extend(option.as_object().unwrap().clone());
        let request = Request::post("/sandboxes")
            .header("x-api-key", "test-token")
            .header("content-type", "application/json")
            .body(Body::from(request_body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected_status);
    }
}

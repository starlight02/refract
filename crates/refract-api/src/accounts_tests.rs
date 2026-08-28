use crate::http_test::{TestRequest, bootstrap_state};
use crate::state::AppState;

const PASSWORD: &str = "Passw0rd1234";

fn data(response: &crate::http_test::TestResponse) -> serde_json::Value {
    response.json()["data"].clone()
}

async fn register(state: &AppState, email: &str) -> i64 {
    let response = TestRequest::post("/api/auth/register")
        .json(&serde_json::json!({
            "email": email,
            "password": PASSWORD,
            "display_name": "Tester",
        }))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200, "{}", response.json());
    data(&response)["user_id"].as_i64().expect("user_id")
}

async fn dev_code(state: &AppState, email: &str) -> String {
    let response = TestRequest::get(&format!("/api/auth/dev-codes?email={email}"))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200, "{}", response.json());
    data(&response)["code"]
        .as_str()
        .expect("dev code")
        .to_owned()
}

async fn verify_email(state: &AppState, email: &str) {
    let code = dev_code(state, email).await;
    let response = TestRequest::post("/api/auth/verify-email")
        .json(&serde_json::json!({ "email": email, "code": code }))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200, "{}", response.json());
}

async fn login(state: &AppState, email: &str) -> crate::http_test::TestResponse {
    TestRequest::post("/api/auth/login")
        .json(&serde_json::json!({
            "email": email,
            "password": PASSWORD,
        }))
        .send(state.clone())
        .await
}

#[tokio::test]
async fn register_verify_login_then_profile() {
    let state = bootstrap_state(false).await;
    let email = "alice@example.com";
    let user_id = register(&state, email).await;
    verify_email(&state, email).await;
    let logged_in = login(&state, email).await;
    assert_eq!(logged_in.status(), 200, "{}", logged_in.json());
    let body = data(&logged_in);
    assert_eq!(body["authenticated"], true);
    assert_ne!(body["restricted"], true);
    let cookie = logged_in.session_cookie().expect("session cookie");

    let profile = TestRequest::get("/api/me/profile")
        .header("cookie", cookie)
        .send(state)
        .await;
    assert_eq!(profile.status(), 200, "{}", profile.json());
    let profile = data(&profile);
    assert_eq!(profile["id"], user_id);
    assert_eq!(profile["email"], email);
    assert_eq!(profile["role"], "user");
    assert_eq!(profile["status"], "active");
}

#[tokio::test]
async fn unverified_login_is_restricted_and_gateway_key_is_forbidden() {
    let state = bootstrap_state(true).await;
    let email = "pending@example.com";
    let user_id = register(&state, email).await;
    let logged_in = login(&state, email).await;
    assert_eq!(logged_in.status(), 200, "{}", logged_in.json());
    assert_eq!(data(&logged_in)["restricted"], true);

    let (key, plaintext) = state
        .key_repo()
        .create_for_user(
            refract_core::DEFAULT_OWNER_ID,
            user_id,
            refract_store::NewApiKey {
                name: "pending".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(key.user_id, Some(user_id));

    let denied = TestRequest::get("/v1/models")
        .header("authorization", format!("Bearer {plaintext}"))
        .send(state)
        .await;
    assert_eq!(denied.status(), 403, "{}", denied.json());
}

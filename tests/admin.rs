use conduit::{Result, Server};
use ruma::UserId;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::OnceCell;

static SERVER: OnceCell<Arc<Server>> = OnceCell::const_new();

async fn server() -> &'static Arc<Server> {
    SERVER
        .get_or_init(|| async {
            let server = Server::new_for_testing().await;
            Arc::new(server)
        })
        .await
}

#[tokio::test]
async fn test_admin_panel_auth() {
    let server = server().await;
    let test_user = server.test_user();
    let test_password = "password";

    // 1. Create an admin user
    let admin_user_id =
        UserId::parse_with_server_name(test_user.username(), server.config.server_name.as_str())
            .unwrap();
    server
        .users
        .create(&admin_user_id, Some(test_password))
        .unwrap();
    server.users.make_admin(&admin_user_id, true).unwrap();

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();

    // 2. Attempt to access protected endpoint without auth
    let unauthorized_res = client
        .get(format!("{}/_conduit/api/users", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized_res.status(), 401);

    // 3. Log in
    let login_res = client
        .post(format!("{}/_conduit/login", server.base_url))
        .json(&serde_json::json!({
            "username": admin_user_id.localpart(),
            "password": test_password,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(login_res.status(), 200);

    // 4. Access protected endpoint with auth
    let authorized_res = client
        .get(format!("{}/_conduit/api/users", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(authorized_res.status(), 200);
    let body: Vec<String> = authorized_res.json().await.unwrap();
    assert!(body.contains(&admin_user_id.to_string()));

    // 5. Log out
    let logout_res = client
        .post(format!("{}/_conduit/logout", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(logout_res.status(), 200);

    // 6. Attempt to access protected endpoint again
    let final_res = client
        .get(format!("{}/_conduit/api/users", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(final_res.status(), 401);
}

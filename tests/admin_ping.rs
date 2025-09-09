use conduit::Server;
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
async fn test_admin_ping() {
    let server = server().await;
    let client = reqwest::Client::new();

    let res = client
        .get(format!("{}/_conduit/ping", server.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "pong");
}

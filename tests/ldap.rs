// An integration test for LDAP authentication.
//
// This test is designed to be run with `cargo test --test ldap`.
// It requires Docker and docker-compose to be installed and running.
//
// The test will:
// 1. Start an OpenLDAP container.
// 2. Add a test user to the LDAP directory.
// 3. Create a temporary Conduit server configuration.
// 4. Start the Conduit server on a random, available port.
// 5. Run a series of login tests against the server.
// 6. Clean up all resources (Docker container, temp files) upon completion.

use conduit::{Config, KeyValueDatabase};
use once_cell::sync::Lazy;
use reqwest::StatusCode;
use serde_json::json;
use std::{
    net::SocketAddr,
    process::{Command, Stdio},
    sync::Mutex,
    time::Duration,
};
use tokio::time::sleep;

// Use a global mutex to ensure that the setup and teardown logic runs only once,
// even if multiple tests are run in parallel.
static LDAP_SETUP: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

struct TestHarness {
    server_address: SocketAddr,
    config: Config,
}

// A guard that ensures the Docker container is shut down when the test harness is dropped.
struct DockerGuard;

impl Drop for DockerGuard {
    fn drop(&mut self) {
        println!("--- Tearing down LDAP container ---");
        let status = Command::new("docker-compose")
            .arg("-f")
            .arg("docker-compose.ldap.yml")
            .arg("down")
            .arg("-v") // Remove volumes
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("Failed to execute docker-compose down");
        assert!(status.success(), "Failed to tear down LDAP container");
    }
}

async fn setup() -> (TestHarness, DockerGuard) {
    let _guard = LDAP_SETUP.lock().unwrap();

    // 1. Start the LDAP server
    println!("--- Setting up LDAP container ---");
    let compose_status = Command::new("docker-compose")
        .arg("-f")
        .arg("docker-compose.ldap.yml")
        .arg("up")
        .arg("-d")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("Failed to execute docker-compose up");
    assert!(compose_status.success(), "Failed to start LDAP container");

    // Give the container a moment to initialize
    sleep(Duration::from_secs(5)).await;

    // 2. Add the test user
    let ldapadd_status = Command::new("docker")
        .args([
            "exec",
            "-i",
            "conduit-openldap-1", // This must match the container name in docker-compose.ldap.yml
            "ldapadd",
            "-x",
            "-D",
            "cn=admin,dc=conduit,dc=rs",
            "-w",
            "admin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            let stdin = child.stdin.as_mut().unwrap();
            std::io::Write::write_all(stdin, include_bytes!("../user.ldif"))?;
            child.wait()
        })
        .expect("Failed to execute ldapadd");
    assert!(
        ldapadd_status.success(),
        "Failed to add user to LDAP server"
    );

    // 3. Configure Conduit
    let db_path = tempfile::tempdir().expect("Failed to create temp dir");
    let mut config = Config::default();
    config.server_name = "localhost".try_into().expect("should be a valid server name");
    config.database_path = db_path.path().to_str().expect("path is valid unicode").to_owned();
    config.port = 0; // Use a random available port
    config.log = "warn,conduit=info".to_owned();

    config.ldap.enabled = true;
    config.ldap.uri = "ldap://localhost:389".to_owned();
    config.ldap.bind_dn = "cn=admin,dc=conduit,dc=rs".to_owned();
    config.ldap.bind_password = "admin".to_owned();
    config.ldap.base_dn = "ou=users,dc=conduit,dc=rs".to_owned();
    config.ldap.user_filter = "(uid=%u)".to_owned();
    config.ldap.attribute_mapping.insert("localpart".to_owned(), "uid".to_owned());
    config.ldap.attribute_mapping.insert("displayname".to_owned(), "cn".to_owned());
    config.ldap.attribute_mapping.insert("email".to_owned(), "mail".to_owned());

    // 4. Start Conduit Server
    KeyValueDatabase::load_or_create(config.clone())
        .await
        .expect("Failed to load database");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let server_address = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, conduit::routes(&Config::default()).into_make_service())
            .await
            .unwrap();
    });

    println!("--- Conduit server started on {} ---", server_address);

    (
        TestHarness {
            server_address,
            config,
        },
        DockerGuard,
    )
}

#[tokio::test]
async fn test_ldap_authentication_flow() {
    let (harness, _docker_guard) = setup().await;
    let base_url = format!("http://{}", harness.server_address);

    // Test Case 1: First-time successful login
    println!("--- Running Test Case 1: First-time successful login ---");
    let client = reqwest::Client::new();
    let res = client
        .post(format!("{}/_matrix/client/v3/login", base_url))
        .json(&json!({
            "type": "m.login.password",
            "identifier": {
                "type": "m.id.user",
                "user": "testuser"
            },
            "password": "password",
            "device_id": "LDAP_TEST_DEVICE"
        }))
        .send()
        .await
        .expect("Request failed");
    assert_eq!(res.status(), StatusCode::OK, "Expected successful login");
    let body: serde_json::Value = res.json().await.expect("Failed to parse response body");
    assert_eq!(body["user_id"], "@testuser:localhost");

    // Test Case 2: Login with incorrect password
    println!("--- Running Test Case 2: Incorrect password ---");
    let client = reqwest::Client::new(); // Use a new client to ensure no session state
    let res = client
        .post(format!("{}/_matrix/client/v3/login", base_url))
        .json(&json!({
            "type": "m.login.password",
            "identifier": {
                "type": "m.id.user",
                "user": "testuser"
            },
            "password": "wrongpassword",
        }))
        .send()
        .await
        .expect("Request failed");
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "Expected failed login");
    let body: serde_json::Value = res.json().await.expect("Failed to parse response body");
    assert_eq!(body["errcode"], "M_FORBIDDEN");

    // Test Case 3: Non-existent user
    println!("--- Running Test Case 3: Non-existent user ---");
    let client = reqwest::Client::new(); // Use a new client
    let res = client
        .post(format!("{}/_matrix/client/v3/login", base_url))
        .json(&json!({
            "type": "m.login.password",
            "identifier": {
                "type": "m.id.user",
                "user": "nosuchuser"
            },
            "password": "password",
        }))
        .send()
        .await
        .expect("Request failed");
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "Expected failed login");
    let body: serde_json::Value = res.json().await.expect("Failed to parse response body");
    assert_eq!(body["errcode"], "M_FORBIDDEN");
}

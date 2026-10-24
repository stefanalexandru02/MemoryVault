use std::time::Duration;

use memvault_core::tei::{Embedder, TeiClient, TeiError, TeiHealth};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(embed: &str, rerank: &str) -> TeiClient {
    TeiClient::new(embed, rerank, "m", 2, Duration::from_secs(2), Duration::from_millis(250))
}

#[tokio::test]
async fn embed_and_rerank_order() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([[0.1, 0.2], [0.3, 0.4]])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([{"index": 1, "score": 0.9}, {"index": 0, "score": 0.1}])),
        )
        .mount(&server)
        .await;
    let tei = client(&server.uri(), &format!("{}/", server.uri()));
    assert_eq!(tei.embed(&["a".into(), "b".into()]).await.unwrap(), vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
    assert_eq!(tei.rerank("q", &["x".into(), "y".into()]).await.unwrap(), vec![0.1, 0.9]);
    assert!(tei.embed(&[]).await.unwrap().is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn errors_timeouts_and_health() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/embed")).respond_with(ResponseTemplate::new(503)).mount(&server).await;
    Mock::given(method("GET")).and(path("/health")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let tei = client(&server.uri(), "http://127.0.0.1:1");
    assert!(matches!(tei.rerank("q", &["x".into()]).await, Err(TeiError::Failed(_))), "unreachable rerank");
    let slow = client(&server.uri(), &server.uri());
    assert!(matches!(slow.rerank("q", &["x".into()]).await, Err(TeiError::Timeout(_))));
    assert!(matches!(tei.embed(&["a".into()]).await, Err(TeiError::Failed(_))));
    assert_eq!(tei.health().await, TeiHealth { embed: true, rerank: false });
}

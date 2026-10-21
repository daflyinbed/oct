//! Startup payload, HTTP caching and streaming regression coverage.
mod support;

use std::io::Read;

use futures_util::StreamExt;
use reqwest::header;
use serde_json::{Value, json};
use support::TestApp;
use support::llm::{self, LlmTurn, finish_chunk, text_chunk};

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut decoded)
        .unwrap();
    decoded
}

#[tokio::test]
async fn configured_catalog_excludes_unconnected_providers_and_disabled_models() {
    let app = TestApp::new().await;
    for (id, key) in [("connected", "test-key"), ("unconnected", "")] {
        assert_eq!(
            app.post_json(
                "/api/providers",
                json!({
                    "id": id, "name": id, "base_url": "http://127.0.0.1:9", "api_key": key,
                })
            )
            .await
            .status(),
            201
        );
        for model in ["enabled", "disabled"] {
            assert_eq!(
                app.post_json(
                    &format!("/api/providers/{id}/models"),
                    json!({
                        "model_id": model, "name": model, "reasoning": true,
                        "reasoning_options": "[{\"type\":\"effort\",\"values\":[\"high\"]}]",
                        "limit_context": 32000,
                    })
                )
                .await
                .status(),
                201
            );
        }
        assert_eq!(
            app.put_json(
                &format!("/api/providers/{id}/models/disabled"),
                json!({"is_enabled": false})
            )
            .await
            .status(),
            200
        );
    }
    let full: Vec<Value> = app.get_json("/api/providers").await.json().await.unwrap();
    assert_eq!(full.len(), 2);
    assert!(
        full.iter()
            .all(|p| p["models"].as_array().unwrap().len() == 2)
    );
    let configured: Vec<Value> = app
        .get_json("/api/providers?configured=true")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(configured.len(), 1);
    assert_eq!(configured[0]["id"], "connected");
    assert!(configured[0].get("api_key").is_none());
    let models = configured[0]["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["model_id"], "enabled");
    assert_eq!(models[0]["limit_context"], 32000);
    assert_eq!(models[0]["reasoning_options"][0]["values"][0], "high");
    app.put_json("/api/providers/connected", json!({"api_key": ""}))
        .await;
    let empty: Vec<Value> = app
        .get_json("/api/providers?configured=true")
        .await
        .json()
        .await
        .unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn json_negotiates_gzip_and_roundtrips() {
    let app = TestApp::new().await;
    let url = format!("{}/api/openapi.json", app.base_url);
    let client = reqwest::Client::new();
    let plain = client
        .get(&url)
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .unwrap();
    assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
    let plain = plain.bytes().await.unwrap();
    let compressed = client
        .get(&url)
        .header(header::ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(compressed.headers()[header::CONTENT_ENCODING], "gzip");
    assert!(
        compressed.headers()[header::VARY]
            .to_str()
            .unwrap()
            .contains("accept-encoding")
    );
    let compressed = compressed.bytes().await.unwrap();
    assert!(compressed.len() < plain.len());
    assert_eq!(gunzip(&compressed), plain);
}

#[tokio::test]
async fn static_assets_compress_and_cache_while_html_revalidates() {
    // Rust-only builds intentionally support an absent frontend bundle.
    if !oct_agent::api::embed::frontend_available() {
        return;
    }
    let app = TestApp::new().await;
    let client = reqwest::Client::new();
    for path in ["/", "/conversation/deep-link"] {
        let response = client
            .get(format!("{}{path}", app.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    }
    let html = app.get_json("/").await.text().await.unwrap();
    let asset = html
        .split("src=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert!(asset.starts_with("/assets/"));
    let url = format!("{}{asset}", app.base_url);
    let plain = client
        .get(&url)
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let compressed = client
        .get(&url)
        .header(header::ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(compressed.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(
        compressed.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(gunzip(&compressed.bytes().await.unwrap()), plain);
    let range = client
        .get(&url)
        .header(header::RANGE, "bytes=0-15")
        .header(header::ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(range.status(), 206);
    assert!(range.headers().get(header::CONTENT_ENCODING).is_none());
    assert_eq!(range.bytes().await.unwrap(), &plain[..16]);
    let missing = app.get_json("/assets/missing-12345678.js").await;
    assert_eq!(missing.headers()[header::CACHE_CONTROL], "no-cache");
    assert_eq!(app.get_json("/api/unknown").await.status(), 404);
}

#[tokio::test]
async fn sse_stays_uncompressed_when_client_accepts_gzip() {
    let app = TestApp::new().await;
    let mock = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("streaming response remains immediately readable"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&mock).await;
    let project = app.create_project().await;
    let conversation = app.create_conversation(&project).await;
    let response = reqwest::Client::new()
        .post(format!(
            "{}/api/conversations/{conversation}/messages",
            app.base_url
        ))
        .header(header::ACCEPT_ENCODING, "gzip")
        .json(&json!({"content": "hello", "provider_spec": "mock-llm:test-model"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
    let events: Vec<_> = support::sse_payloads(response).collect().await;
    assert!(events.iter().any(|event| event["type"] == "text_delta"));
}

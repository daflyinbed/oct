//! Real PTY + WebSocket tests; no model or external SSH server is needed.
#![cfg(unix)]
mod support;

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use futures_util::{SinkExt, StreamExt};
use oct_agent::api::terminals::CreateTerminalRequest;
use oct_agent::api::{projects::delete_project, terminals::create_terminal};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use support::TestApp;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn next(socket: &mut Socket) -> Message {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("terminal event deadline")
            .expect("terminal stream ended")
            .expect("terminal event");
        if let Message::Ping(bytes) = message {
            socket.send(Message::Pong(bytes)).await.unwrap();
            continue;
        }
        return message;
    }
}

fn output(message: Message, cursor: &mut u64, bytes: &mut Vec<u8>) {
    if let Message::Binary(frame) = message {
        assert!(frame.len() >= 8);
        let offset = u64::from_be_bytes(frame[..8].try_into().unwrap());
        assert_eq!(offset, *cursor, "no skipped or duplicated PTY output");
        bytes.extend_from_slice(&frame[8..]);
        *cursor += (frame.len() - 8) as u64;
    }
}

async fn until(socket: &mut Socket, cursor: &mut u64, marker: &str) -> String {
    let mut bytes = Vec::new();
    loop {
        let message = next(socket).await;
        if let Message::Text(text) = &message {
            let value: Value = serde_json::from_str(text).unwrap();
            assert_ne!(value["type"], "error", "{value}");
            assert_ne!(value["type"], "exit", "shell exited before {marker}");
        }
        output(message, cursor, &mut bytes);
        if String::from_utf8_lossy(&bytes).contains(marker) {
            return String::from_utf8_lossy(&bytes).into_owned();
        }
    }
}

async fn command(socket: &mut Socket, value: &str) {
    socket
        .send(Message::Binary(value.as_bytes().to_vec().into()))
        .await
        .unwrap();
}

fn ws_url(app: &TestApp, project: &str, id: &str, after: u64) -> String {
    format!(
        "{}/api/projects/{project}/terminals/{id}/ws?after={after}",
        app.base_url.replace("http://", "ws://")
    )
}

async fn connect_terminal(url: &str) -> Socket {
    // A completed close handshake can precede the old handler dropping its
    // ownership permit. Yield on that specific 409, with a bounded deadline.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match connect_async(url).await {
                Ok((socket, _)) => break socket,
                Err(tokio_tungstenite::tungstenite::Error::Http(response))
                    if response.status() == 409 =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("attach terminal: {error}"),
            }
        }
    })
    .await
    .unwrap()
}

async fn attach(url: &str, cursor: &mut u64) -> Socket {
    let mut socket = connect_terminal(url).await;
    let ready = next(&mut socket).await;
    let Message::Text(ready) = ready else {
        panic!("expected ready");
    };
    let ready: Value = serde_json::from_str(&ready).unwrap();
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["offset"], *cursor);
    let mut replay = Vec::new();
    loop {
        let event = next(&mut socket).await;
        if let Message::Text(text) = &event {
            let value: Value = serde_json::from_str(text).unwrap();
            if value["type"] == "live" {
                break;
            }
            panic!("unexpected attachment control: {value}");
        }
        output(event, cursor, &mut replay);
    }
    socket
}

#[tokio::test]
async fn shell_input_resize_interrupt_reconnect_and_exit() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let path = format!("/api/projects/{project}/terminals");
    let response = app.post_json(&path, json!({"cols":80,"rows":24})).await;
    assert_eq!(response.status(), 201);
    let info: Value = response.json().await.unwrap();
    let id = info["id"].as_str().unwrap();
    assert_eq!(
        info["working_dir"],
        app.dir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    let mut cursor = 0;
    let mut socket = attach(&ws_url(&app, &project, id, cursor), &mut cursor).await;
    command(&mut socket, "stty -echo; printf '\\n__READY__\\n'\r").await;
    until(&mut socket, &mut cursor, "\r\n__READY__\r\n").await;

    socket
        .send(Message::Text(
            json!({"type":"resize","cols":120,"rows":40})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    command(
        &mut socket,
        "stty size; pwd; OCT_TEST_VALUE='中文终端'; printf '__SIZE__\\n'\r",
    )
    .await;
    let text = until(&mut socket, &mut cursor, "__SIZE__\r\n").await;
    assert!(text.contains("40 120"), "{text}");
    assert!(text.contains(app.dir.path().to_str().unwrap()), "{text}");

    // The same session cannot acquire a second input controller.
    let duplicate = connect_async(ws_url(&app, &project, id, cursor))
        .await
        .unwrap_err();
    assert!(
        matches!(duplicate, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 409)
    );

    socket.close(None).await.unwrap();
    let url = ws_url(&app, &project, id, cursor);
    let mut socket = attach(&url, &mut cursor).await;
    command(
        &mut socket,
        "printf '%s\\n' \"$OCT_TEST_VALUE\"; printf '__RESUMED__\\n'\r",
    )
    .await;
    let text = until(&mut socket, &mut cursor, "__RESUMED__\r\n").await;
    assert!(
        text.contains("中文终端"),
        "shell state survived reconnect: {text}"
    );

    command(&mut socket, "cat\r").await;
    command(&mut socket, "__CAT_IS_RUNNING__\n").await;
    until(&mut socket, &mut cursor, "__CAT_IS_RUNNING__\r\n").await;
    command(&mut socket, "\u{3}").await;
    command(&mut socket, "printf '__INTERRUPTED__\\n'\r").await;
    until(&mut socket, &mut cursor, "__INTERRUPTED__\r\n").await;

    command(&mut socket, "exit 7\r").await;
    loop {
        let event = next(&mut socket).await;
        if let Message::Text(text) = &event {
            let value: Value = serde_json::from_str(text).unwrap();
            if value["type"] == "exit" {
                assert_eq!(value["exit_code"], 7);
                break;
            }
            assert_ne!(value["type"], "error", "{value}");
        }
        output(event, &mut cursor, &mut Vec::new());
    }
    let items: Vec<Value> = app.get_json(&path).await.json().await.unwrap();
    assert_eq!(items[0]["exited"], true);
    assert_eq!(items[0]["exit_code"], 7);
    assert_eq!(app.http_delete(&format!("{path}/{id}")).await.status(), 204);
    assert!(
        app.get_json(&path)
            .await
            .json::<Vec<Value>>()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn output_acknowledgements_distinguish_old_replay_from_unread_queries() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let path = format!("/api/projects/{project}/terminals");
    let info: Value = app.post_json(&path, json!({})).await.json().await.unwrap();
    let id = info["id"].as_str().unwrap();
    let mut cursor = 0;
    let mut socket = attach(&ws_url(&app, &project, id, cursor), &mut cursor).await;
    command(&mut socket, "stty -echo; printf '\\033[0c\\n__SEEN__\\n'\r").await;
    until(&mut socket, &mut cursor, "\r\n__SEEN__\r\n").await;
    let acknowledged = cursor;
    socket
        .send(Message::Text(
            json!({"type":"ack", "offset":acknowledged})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    // An older ack (as when a fresh renderer replays historical chunks) must
    // not move the processed watermark backwards.
    socket
        .send(Message::Text(
            json!({"type":"ack", "offset":0}).to_string().into(),
        ))
        .await
        .unwrap();
    command(&mut socket, "printf '\\033[5n\\n__UNREAD__\\n'\r").await;
    until(&mut socket, &mut cursor, "\r\n__UNREAD__\r\n").await;
    socket.close(None).await.unwrap();

    // Simulate a page refresh from byte zero; the readiness frame preserves
    // the confirmed boundary despite the unread output and previous stdin.
    let mut refreshed = connect_terminal(&ws_url(&app, &project, id, 0)).await;
    let Message::Text(ready) = next(&mut refreshed).await else {
        panic!("expected ready");
    };
    let ready: Value = serde_json::from_str(&ready).unwrap();
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["offset"], 0);
    assert_eq!(ready["replay_reply_after"], acknowledged);
    assert!(ready["replay_reply_after"].as_u64().unwrap() < cursor);
    let mut replay_cursor = 0;
    let replay = until(&mut refreshed, &mut replay_cursor, "\r\n__UNREAD__\r\n").await;
    assert!(replay.contains("\u{1b}[0c"));
    assert!(replay.contains("\u{1b}[5n"));

    // A client cannot acknowledge bytes the attachment has not sent.
    refreshed
        .send(Message::Text(
            json!({"type":"ack", "offset":u64::MAX}).to_string().into(),
        ))
        .await
        .unwrap();
    loop {
        if let Message::Text(text) = next(&mut refreshed).await {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["type"] == "error" {
                assert!(
                    value["message"]
                        .as_str()
                        .unwrap()
                        .contains("acknowledgement")
                );
                break;
            }
        }
    }
    assert_eq!(app.http_delete(&format!("{path}/{id}")).await.status(), 204);
}

#[tokio::test]
async fn terminals_reject_cross_origin_invalid_size_and_wrong_project() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let other = app.create_project().await;
    let path = format!("/api/projects/{project}/terminals");
    assert_eq!(
        app.post_json(&path, json!({"cols":0,"rows":24}))
            .await
            .status(),
        400
    );
    let client = reqwest::Client::new();
    let blocked = client
        .post(format!("{}{path}", app.base_url))
        .header("Origin", "https://evil.example")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), 400);
    assert_eq!(
        app.post_json("/api/projects/missing/terminals", json!({}))
            .await
            .status(),
        404
    );
    let info: Value = app.post_json(&path, json!({})).await.json().await.unwrap();
    let id = info["id"].as_str().unwrap();
    assert_eq!(
        app.http_delete(&format!("/api/projects/{other}/terminals/{id}"))
            .await
            .status(),
        404
    );
    let wrong = connect_async(ws_url(&app, &other, id, 0))
        .await
        .unwrap_err();
    assert!(
        matches!(wrong, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 404)
    );
    let mut request = ws_url(&app, &project, id, 0).into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", "https://evil.example".parse().unwrap());
    let blocked = connect_async(request).await.unwrap_err();
    assert!(
        matches!(blocked, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 400)
    );
    assert_eq!(app.http_delete(&format!("{path}/{id}")).await.status(), 204);
}

#[tokio::test]
async fn deleting_project_removes_its_live_terminal() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let info: Value = app
        .post_json(&format!("/api/projects/{project}/terminals"), json!({}))
        .await
        .json()
        .await
        .unwrap();
    let id = info["id"].as_str().unwrap();
    assert_eq!(
        app.http_delete(&format!("/api/projects/{project}"))
            .await
            .status(),
        204
    );
    let gone = connect_async(ws_url(&app, &project, id, 0))
        .await
        .unwrap_err();
    assert!(
        matches!(gone, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 404)
    );
}

#[tokio::test]
async fn deleting_project_waits_for_terminal_creation() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let mut create = Box::pin(create_terminal(
        State(app.state.clone()),
        Path(project.clone()),
        HeaderMap::new(),
        Json(CreateTerminalRequest { cols: 80, rows: 24 }),
    ));
    // The first database await pauses creation after admission, without sleeps.
    assert!(futures_util::poll!(create.as_mut()).is_pending());
    assert!(app.state.runs.lifecycle.admission.try_write().is_err());
    let mut delete = Box::pin(delete_project(
        State(app.state.clone()),
        Path(project.clone()),
    ));
    assert!(futures_util::poll!(delete.as_mut()).is_pending());

    let (created, deleted) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(create, delete)
    })
    .await
    .unwrap();
    let (status, Json(info)) = created.unwrap();
    assert_eq!(status, 201);
    assert_eq!(deleted.unwrap(), 204);
    assert!(app.state.terminals.list(&project).is_empty());
    let gone = connect_async(ws_url(&app, &project, &info.id, 0))
        .await
        .unwrap_err();
    assert!(
        matches!(gone, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 404)
    );
}

#[tokio::test]
async fn terminal_creation_after_project_deletion_returns_not_found() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let admission = app.state.runs.lifecycle.admission.write().await;
    let mut delete = Box::pin(delete_project(
        State(app.state.clone()),
        Path(project.clone()),
    ));
    assert!(futures_util::poll!(delete.as_mut()).is_pending());
    let mut create = Box::pin(create_terminal(
        State(app.state.clone()),
        Path(project.clone()),
        HeaderMap::new(),
        Json(CreateTerminalRequest { cols: 80, rows: 24 }),
    ));
    assert!(futures_util::poll!(create.as_mut()).is_pending());
    // The queued writer wins admission; creation must look up the project
    // only after deletion commits, rather than keeping a stale project row.
    drop(admission);
    let (deleted, created) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(delete, create)
    })
    .await
    .unwrap();
    assert_eq!(deleted.unwrap(), 204);
    assert!(matches!(
        created,
        Err(oct_agent::error::AppError::NotFound(_))
    ));
    assert!(app.state.terminals.list(&project).is_empty());
}

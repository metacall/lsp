//! End-to-end protocol test over an in-memory connection.
//!
//! Diagnostics are pull only, and a request the current snapshot cannot answer
//! fails with ContentModified (-32801).

#![expect(clippy::unwrap_used, reason = "a test may abort on setup failure")]
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request as WireRequest, RequestId};
use lsp_types::{
    ClientCapabilities, DiagnosticClientCapabilities, DidChangeTextDocumentParams,
    DidOpenTextDocumentParams, DocumentDiagnosticParams, DocumentSymbolParams, HoverParams,
    InitializeParams, Position, TextDocumentClientCapabilities, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
    VersionedTextDocumentIdentifier, WorkspaceFolder,
};
use meta_call_lsp::server::run_connection;
use serde_json::Value;

const APP: &str = "def greet(name):\n    \"\"\"Say hi.\"\"\"\n    return name\n\n\ndef caller():\n    return greet(\"x\")\n";
const CONTENT_MODIFIED: i32 = -32801;
const INVALID_PARAMS: i32 = -32602;

fn uri(text: &str) -> Uri {
    text.parse().expect("uri")
}

fn send(client: &Connection, message: Message) {
    client.sender.send(message).expect("send");
}

fn request(id: i32, method: &str, params: Value) -> Message {
    Message::Request(WireRequest {
        id: RequestId::from(id),
        method: method.to_string(),
        params,
    })
}

fn notification(method: &str, params: Value) -> Message {
    Message::Notification(Notification {
        method: method.to_string(),
        params,
    })
}

/// Wait for one response and return its result or its error code.
fn response_for(client: &Connection, id: i32) -> Result<Value, i32> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let message = client
            .receiver
            .recv_timeout(remaining)
            .expect("server message");
        if let Message::Response(response) = message
            && response.id == RequestId::from(id)
        {
            return response.response_result.map_err(|error| error.code);
        }
    }
}

/// Request diagnostics until the snapshot covers the current document version.
fn diagnostics(client: &Connection, app_uri: &str, mut id: i32) -> (Value, i32) {
    for _ in 0..100 {
        send(
            client,
            request(
                id,
                "textDocument/diagnostic",
                serde_json::to_value(DocumentDiagnosticParams {
                    text_document: TextDocumentIdentifier { uri: uri(app_uri) },
                    identifier: None,
                    previous_result_id: None,
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                })
                .unwrap(),
            ),
        );
        match response_for(client, id) {
            Ok(report) => return (report, id),
            Err(CONTENT_MODIFIED) => {
                id += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(other) => panic!("unexpected error code {other}"),
        }
    }
    panic!("diagnostics never became current");
}

fn initialize_params(root: &str, pull_diagnostics: bool) -> InitializeParams {
    InitializeParams {
        process_id: None,
        capabilities: ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                diagnostic: pull_diagnostics.then(DiagnosticClientCapabilities::default),
                ..Default::default()
            }),
            ..Default::default()
        },
        workspace_folders: Some(vec![WorkspaceFolder {
            uri: uri(root),
            name: "workspace".to_string(),
        }]),
        ..Default::default()
    }
}

#[test]
fn initialize_to_exit_over_an_in_memory_connection() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("a.py");
    std::fs::write(&app, APP).unwrap();
    let root = url::Url::from_directory_path(dir.path())
        .unwrap()
        .to_string();
    let app_uri = url::Url::from_file_path(&app).unwrap().to_string();

    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || run_connection(server));

    let params = initialize_params(&root, true);
    send(
        &client,
        request(1, "initialize", serde_json::to_value(params).unwrap()),
    );
    let capabilities = response_for(&client, 1).expect("initialize result");
    assert_eq!(
        capabilities["capabilities"]["textDocumentSync"]["change"],
        2
    );
    assert_eq!(capabilities["capabilities"]["hoverProvider"], true);
    assert_eq!(
        capabilities["capabilities"]["completionProvider"]["triggerCharacters"][0],
        "."
    );
    assert_eq!(
        capabilities["capabilities"]["diagnosticProvider"]["identifier"],
        "meta-ast"
    );
    assert_eq!(
        capabilities["capabilities"]["diagnosticProvider"]["interFileDependencies"],
        false
    );

    send(&client, notification("initialized", serde_json::json!({})));
    send(
        &client,
        notification(
            "textDocument/didOpen",
            serde_json::to_value(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri(&app_uri),
                    language_id: "python".to_string(),
                    version: 1,
                    text: APP.to_string(),
                },
            })
            .unwrap(),
        ),
    );

    let (report, last_id) = diagnostics(&client, &app_uri, 10);
    assert_eq!(report["kind"], "full");
    assert!(report["items"].is_array());
    let result_id = report["resultId"].as_str().expect("result id").to_string();
    assert!(result_id.starts_with("meta-ast:"));

    let id = last_id + 1;
    send(
        &client,
        request(
            id,
            "textDocument/diagnostic",
            serde_json::to_value(DocumentDiagnosticParams {
                text_document: TextDocumentIdentifier { uri: uri(&app_uri) },
                identifier: None,
                previous_result_id: Some(result_id),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        ),
    );
    let unchanged = response_for(&client, id).expect("unchanged report");
    assert_eq!(unchanged["kind"], "unchanged");

    send(
        &client,
        request(
            2,
            "textDocument/hover",
            serde_json::to_value(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri(&app_uri) },
                    position: Position {
                        line: 0,
                        character: 5,
                    },
                },
                work_done_progress_params: Default::default(),
            })
            .unwrap(),
        ),
    );
    let hover = response_for(&client, 2).expect("hover result");
    assert!(
        hover["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("greet")
    );

    // A full-text change makes the snapshot stale: the server answers ContentModified.
    let updated = format!("{APP}\n\ndef extra(): pass\n");
    send(
        &client,
        notification(
            "textDocument/didChange",
            serde_json::to_value(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri(&app_uri),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: updated,
                }],
            })
            .unwrap(),
        ),
    );
    let (_report, last_id) = diagnostics(&client, &app_uri, last_id + 2);

    let id = last_id + 1;
    send(
        &client,
        request(
            id,
            "textDocument/documentSymbol",
            serde_json::to_value(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri(&app_uri) },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        ),
    );
    let symbols = response_for(&client, id).expect("documentSymbol result");
    let names: Vec<&str> = symbols
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect();
    assert!(names.contains(&"extra"), "symbols: {names:?}");
    assert!(
        symbols[0]["selectionRange"].is_object(),
        "documentSymbol is the nested shape: {symbols:?}"
    );

    send(&client, request(4, "shutdown", Value::Null));
    let shutdown = response_for(&client, 4).expect("shutdown result");
    assert!(shutdown.is_null());
    send(&client, notification("exit", Value::Null));

    handle.join().expect("server thread").expect("clean exit");
}

#[test]
fn initialize_without_pull_diagnostics_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = url::Url::from_directory_path(dir.path())
        .unwrap()
        .to_string();
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || run_connection(server));

    let params = initialize_params(&root, false);
    send(
        &client,
        request(1, "initialize", serde_json::to_value(params).unwrap()),
    );

    let error = response_for(&client, 1).expect_err("initialize must fail");
    assert_eq!(error, INVALID_PARAMS);
    assert!(handle.join().expect("server thread").is_err());
}

#[test]
fn initialize_without_workspace_folders_is_rejected() {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || run_connection(server));

    let mut params = initialize_params("file:///unused", true);
    params.workspace_folders = None;
    send(
        &client,
        request(1, "initialize", serde_json::to_value(params).unwrap()),
    );

    let error = response_for(&client, 1).expect_err("initialize must fail");
    assert_eq!(error, INVALID_PARAMS);
    assert!(handle.join().expect("server thread").is_err());
}

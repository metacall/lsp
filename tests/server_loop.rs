//! End-to-end protocol test over an in-memory connection.

use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::{
    ClientCapabilities, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbolParams, HoverParams, InitializeParams, Position, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
    VersionedTextDocumentIdentifier, WorkspaceFolder,
};
use meta_call_lsp::server::run_connection;
use serde_json::Value;

const APP: &str = "def greet(name):\n    \"\"\"Say hi.\"\"\"\n    return name\n\n\ndef caller():\n    return greet(\"x\")\n";

fn uri(text: &str) -> Uri {
    text.parse().expect("uri")
}

fn send(client: &Connection, message: Message) {
    client.sender.send(message).expect("send");
}

fn request(id: i32, method: &str, params: Value) -> Message {
    Message::Request(Request {
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

fn recv_until<F>(client: &Connection, mut matches: F) -> Value
where
    F: FnMut(&Message) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let message = client
            .receiver
            .recv_timeout(remaining)
            .expect("server message");
        if matches(&message) {
            return match message {
                Message::Response(response) => response.response_result.expect("ok response"),
                Message::Notification(notification) => notification.params,
                Message::Request(request) => panic!("unexpected request {}", request.method),
            };
        }
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

    let params = InitializeParams {
        process_id: None,
        capabilities: ClientCapabilities::default(),
        workspace_folders: Some(vec![WorkspaceFolder {
            uri: uri(&root),
            name: "workspace".to_string(),
        }]),
        ..Default::default()
    };
    send(
        &client,
        request(1, "initialize", serde_json::to_value(params).unwrap()),
    );
    let capabilities = recv_until(
        &client,
        |message| matches!(message, Message::Response(response) if response.id == RequestId::from(1)),
    );
    assert_eq!(
        capabilities["capabilities"]["textDocumentSync"]["change"],
        1
    );
    assert_eq!(capabilities["capabilities"]["hoverProvider"], true);
    assert_eq!(
        capabilities["capabilities"]["completionProvider"]["triggerCharacters"][0],
        "."
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

    let diagnostics = recv_until(&client, |message| {
        matches!(
            message,
            Message::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics"
        )
    });
    assert_eq!(diagnostics["uri"], app_uri);

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
    let hover = recv_until(
        &client,
        |message| matches!(message, Message::Response(response) if response.id == RequestId::from(2)),
    );
    assert!(
        hover["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("greet")
    );

    // A full-text change exercises the patch path and the debounce.
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
    recv_until(&client, |message| {
        matches!(
            message,
            Message::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics"
        )
    });

    send(
        &client,
        request(
            3,
            "textDocument/documentSymbol",
            serde_json::to_value(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri(&app_uri) },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .unwrap(),
        ),
    );
    let symbols = recv_until(
        &client,
        |message| matches!(message, Message::Response(response) if response.id == RequestId::from(3)),
    );
    let names: Vec<&str> = symbols
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect();
    assert!(names.contains(&"extra"), "symbols: {names:?}");

    send(&client, request(4, "shutdown", Value::Null));
    let shutdown = recv_until(
        &client,
        |message| matches!(message, Message::Response(response) if response.id == RequestId::from(4)),
    );
    assert!(shutdown.is_null());
    send(&client, notification("exit", Value::Null));

    handle.join().expect("server thread").expect("clean exit");
}

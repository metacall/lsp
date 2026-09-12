//! Client-visible work-done progress for reindex operations.
use lsp_server::{
    Connection, Message, Notification as WireNotification, Request as WireRequest, RequestId,
    Response,
};
use lsp_types::request::{Request as _, WorkDoneProgressCreate};
use lsp_types::{
    NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin,
    WorkDoneProgressCreateParams, WorkDoneProgressEnd,
};

/// Work-done progress, one operation at a time.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum Progress {
    #[default]
    Idle,
    /// Create request sent, waiting for the client ack.
    Launching { ack: RequestId, seq: u64 },
    /// Client acked; the End notification is still owed.
    Active { seq: u64 },
}

pub(crate) struct ProgressTracker {
    supported: bool,
    progress: Progress,
    token: NumberOrString,
    next_id: u64,
    armed: Option<u64>,
}

impl ProgressTracker {
    pub(crate) fn new(supported: bool) -> Self {
        Self {
            supported,
            progress: Progress::Idle,
            token: NumberOrString::String(String::new()),
            next_id: 0,
            armed: None,
        }
    }

    /// Arm progress for one seq; unsupported clients arm nothing, and every armed request reports once.
    pub(crate) fn arm(&mut self, seq: u64) {
        if self.supported {
            self.armed = Some(seq);
        }
    }

    pub(crate) fn pump(&mut self, connection: &Connection) {
        if self.progress == Progress::Idle
            && let Some(seq) = self.armed.take()
        {
            self.start(connection, seq);
        }
    }

    /// The create request was acked: report Begin. A declined create reports nothing.
    pub(crate) fn on_ack(
        &mut self,
        connection: &Connection,
        response: &Response,
        applied_seq: u64,
    ) {
        let (ack, seq) = match &self.progress {
            Progress::Launching { ack, seq } => (ack.clone(), *seq),
            _ => return,
        };
        if response.id != ack {
            return;
        }
        self.progress = Progress::Idle;
        let fresh = seq > applied_seq;
        if response.response_result.is_ok() && fresh {
            self.send_begin(connection);
            self.progress = Progress::Active { seq };
        }
    }

    /// A reindex response closes progress armed for or tracking that seq.
    pub(crate) fn finish(&mut self, seq: u64, connection: &Connection) {
        if self.armed.is_some_and(|armed| seq >= armed) {
            self.armed = None;
        }
        let closes = match &self.progress {
            Progress::Launching { seq: armed, .. } => seq >= *armed,
            Progress::Active { seq: active } => seq >= *active,
            Progress::Idle => false,
        };
        if closes {
            self.close(connection);
        }
    }

    /// Close any open progress after a worker loss or shutdown.
    pub(crate) fn abandon(&mut self, connection: &Connection) {
        self.close(connection);
    }

    fn close(&mut self, connection: &Connection) {
        if matches!(self.progress, Progress::Active { .. }) {
            self.send_end(connection);
        }
        self.progress = Progress::Idle;
    }

    fn start(&mut self, connection: &Connection, seq: u64) {
        self.next_id += 1;
        let token = NumberOrString::String(format!("meta-ast-reindex-{}", self.next_id));
        let ack = RequestId::from(format!("meta-ast-progress-{}", self.next_id));
        let create = WireRequest {
            id: ack.clone(),
            method: WorkDoneProgressCreate::METHOD.to_string(),
            params: serde_json::to_value(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .unwrap_or(serde_json::Value::Null),
        };
        let _ = connection.sender.send(Message::Request(create));
        self.progress = Progress::Launching { ack, seq };
        self.token = token;
    }

    fn send_begin(&self, connection: &Connection) {
        let begin = ProgressParams {
            token: self.token.clone(),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "Indexing workspace".to_string(),
                cancellable: Some(false),
                message: None,
                percentage: None,
            })),
        };
        let _ = connection
            .sender
            .send(Message::Notification(WireNotification::new(
                "$/progress".to_string(),
                begin,
            )));
    }

    fn send_end(&self, connection: &Connection) {
        let end = ProgressParams {
            token: self.token.clone(),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                message: None,
            })),
        };
        let _ = connection
            .sender
            .send(Message::Notification(WireNotification::new(
                "$/progress".to_string(),
                end,
            )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tracker() -> ProgressTracker {
        let mut tracker = ProgressTracker::new(true);
        tracker.token = NumberOrString::String("meta-ast-reindex-1".to_string());
        tracker
    }

    fn launch(seq: u64) -> (ProgressTracker, RequestId) {
        let mut tracker = tracker();
        let ack = RequestId::from(format!("meta-ast-progress-{seq}"));
        tracker.progress = Progress::Launching {
            ack: ack.clone(),
            seq,
        };
        (tracker, ack)
    }

    fn ack_ok(ack: &RequestId) -> Response {
        Response {
            id: ack.clone(),
            response_result: Ok(serde_json::Value::Null),
        }
    }

    fn next_progress_message(client: &Connection) -> ProgressParams {
        let message = client
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("progress notification");
        let Message::Notification(notification) = message else {
            panic!("expected a progress notification");
        };
        assert_eq!(notification.method, "$/progress");
        serde_json::from_value(notification.params).unwrap()
    }

    fn expect_silence(client: &Connection) {
        assert!(
            client
                .receiver
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
    }

    #[test]
    fn arming_reports_every_request_when_supported() {
        let mut tracker = ProgressTracker::new(true);
        tracker.arm(7);
        assert_eq!(tracker.armed, Some(7));
        tracker.arm(8);
        assert_eq!(tracker.armed, Some(8));
    }

    #[test]
    fn unsupported_clients_never_arm() {
        let mut tracker = ProgressTracker::new(false);
        tracker.arm(7);
        assert_eq!(tracker.armed, None);
    }

    #[test]
    fn acked_create_reports_begin_with_the_launched_token() {
        let (server, client) = Connection::memory();
        let (mut tracker, ack) = launch(7);

        tracker.on_ack(&server, &ack_ok(&ack), 0);

        assert_eq!(tracker.progress, Progress::Active { seq: 7 });
        assert_eq!(
            next_progress_message(&client).token,
            NumberOrString::String("meta-ast-reindex-1".to_string())
        );
    }

    #[test]
    fn declined_create_reports_nothing() {
        let (server, client) = Connection::memory();
        let (mut tracker, ack) = launch(7);
        let declined = Response {
            id: ack,
            response_result: Err(lsp_server::ResponseError {
                code: -32601,
                message: "declined".to_string(),
                data: None,
            }),
        };

        tracker.on_ack(&server, &declined, 0);

        assert_eq!(tracker.progress, Progress::Idle);
        expect_silence(&client);
    }

    #[test]
    fn ack_for_finished_work_reports_nothing() {
        let (server, client) = Connection::memory();
        let (mut tracker, ack) = launch(5);

        tracker.finish(5, &server);
        tracker.on_ack(&server, &ack_ok(&ack), 5);

        assert_eq!(tracker.progress, Progress::Idle);
        expect_silence(&client);
    }

    #[test]
    fn a_response_covers_its_own_and_older_seqs() {
        let (server, client) = Connection::memory();
        let (mut tracker, _) = launch(5);

        tracker.finish(6, &server);

        assert_eq!(
            tracker.progress,
            Progress::Idle,
            "a newer response proves the armed request was merged"
        );
        expect_silence(&client);
    }

    #[test]
    fn active_progress_ends_with_the_token_it_launched() {
        let (server, client) = Connection::memory();
        let (mut tracker, ack) = launch(2);
        tracker.on_ack(&server, &ack_ok(&ack), 0);
        let begin_token = next_progress_message(&client).token;

        tracker.finish(2, &server);

        assert_eq!(tracker.progress, Progress::Idle);
        assert_eq!(next_progress_message(&client).token, begin_token);
    }

    #[test]
    fn ack_for_already_applied_work_reports_nothing() {
        let (server, client) = Connection::memory();
        let (mut tracker, ack) = launch(5);

        tracker.on_ack(&server, &ack_ok(&ack), 5);

        assert_eq!(tracker.progress, Progress::Idle);
        expect_silence(&client);
    }

    #[test]
    fn stale_responses_close_nothing() {
        let (server, _client) = Connection::memory();
        let (mut tracker, ack) = launch(5);
        tracker.on_ack(&server, &ack_ok(&ack), 0);

        tracker.finish(3, &server);

        assert_eq!(tracker.progress, Progress::Active { seq: 5 });
    }

    #[test]
    fn abandon_ends_active_but_not_launching() {
        let (server, client) = Connection::memory();
        let (mut tracker, _) = launch(4);

        tracker.abandon(&server);
        assert_eq!(tracker.progress, Progress::Idle);
        expect_silence(&client);

        tracker.progress = Progress::Active { seq: 4 };
        tracker.abandon(&server);
        assert_eq!(tracker.progress, Progress::Idle);
        next_progress_message(&client);
    }
}

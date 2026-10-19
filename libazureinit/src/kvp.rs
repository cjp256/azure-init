// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Adapts tracing spans and events to libazureinit-kvp diagnostics.

use std::{
    fmt::{self, Write},
    fs::OpenOptions,
    io,
    time::{Duration, Instant},
};

use libazureinit_kvp::{
    write_report, DiagnosticWriter, KvpPoolStore, Outcome, ProvisioningReport,
};
use tokio::{
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{
    field::{Field, Visit},
    span::{Attributes, Id},
    Subscriber,
};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};
use uuid::Uuid;

pub struct Kvp {
    pub(crate) tracing_layer: EmitKVPLayer,
    pub(crate) writer: JoinHandle<io::Result<()>>,
}

enum Message {
    Start {
        id: String,
        name: String,
        payload: String,
    },
    Finish {
        id: String,
        name: String,
        outcome: Outcome,
        duration: Duration,
    },
    Event {
        name: String,
        payload: String,
        outcome: Option<Outcome>,
    },
    Report(ProvisioningReport),
}

impl Kvp {
    pub(crate) fn new(
        store: KvpPoolStore,
        vm_id: &str,
        graceful_shutdown: CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        let diagnostics = DiagnosticWriter::new(
            store.clone(),
            concat!("azure-init/", env!("CARGO_PKG_VERSION")),
            vm_id,
        )?;
        store.clear_if_stale()?;
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(store.path())?;
        let (events_tx, events_rx) = unbounded_channel();
        let writer = tokio::spawn(Self::kvp_writer(
            store,
            diagnostics,
            events_rx,
            graceful_shutdown,
        ));
        Ok(Self {
            tracing_layer: EmitKVPLayer { events_tx },
            writer,
        })
    }

    async fn kvp_writer(
        store: KvpPoolStore,
        diagnostics: DiagnosticWriter,
        mut events: UnboundedReceiver<Message>,
        token: CancellationToken,
    ) -> io::Result<()> {
        let mut first_error = None;
        loop {
            let message = tokio::select! {
                _ = token.cancelled(), if !events.is_closed() => {
                    events.close();
                    continue;
                }
                message = events.recv() => message,
            };
            let Some(message) = message else { break };
            let result = match message {
                Message::Start { id, name, payload } => {
                    diagnostics.emit_start(&id, &name, payload, None)
                }
                Message::Finish {
                    id,
                    name,
                    outcome,
                    duration,
                } => diagnostics
                    .emit_finish(&id, &name, "", None, outcome, duration),
                Message::Event {
                    name,
                    payload,
                    outcome,
                } => {
                    diagnostics.emit_event(&name, payload, None, outcome, None)
                }
                Message::Report(report) => write_report(&store, &report),
            };
            if let Err(error) = result {
                // Logging through tracing here would feed this same queue.
                eprintln!("Failed to write KVP telemetry: {error}");
                first_error.get_or_insert_with(|| io::Error::other(error));
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

struct SpanState {
    id: String,
    started: Instant,
    outcome: Outcome,
}

#[derive(Default)]
struct StringVisitor {
    message: String,
    report: Option<String>,
}

impl Visit for StringVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "health_report" {
            self.report = Some(value.to_owned());
        } else {
            self.record_debug(field, &value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "health_report" {
            self.report = Some(format!("{value:?}"));
            return;
        }
        if !self.message.is_empty() {
            self.message.push_str(", ");
        }
        write!(self.message, "{}={value:?}", field.name())
            .expect("writing to a string cannot fail");
    }
}

/// Queues tracing data for the KVP writer task.
pub struct EmitKVPLayer {
    events_tx: UnboundedSender<Message>,
}

impl EmitKVPLayer {
    fn send(&self, message: Message) {
        if let Err(error) = self.events_tx.send(message) {
            eprintln!("Failed to queue KVP telemetry: {error}");
        }
    }
}

impl<S> Layer<S> for EmitKVPLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        _attrs: &Attributes<'_>,
        id: &Id,
        ctx: Context<'_, S>,
    ) {
        if let Some(span) = ctx.span(id) {
            let state = SpanState {
                id: Uuid::new_v4().to_string(),
                started: Instant::now(),
                outcome: Outcome::Success,
            };
            self.send(Message::Start {
                id: state.id.clone(),
                name: format_span_name(span.metadata()),
                payload: String::new(),
            });
            span.extensions_mut().insert(state);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let failed = *event.metadata().level() == tracing::Level::ERROR;
        if failed {
            if let Some(span) = ctx.lookup_current() {
                for span in span.scope() {
                    if let Some(state) =
                        span.extensions_mut().get_mut::<SpanState>()
                    {
                        state.outcome = Outcome::Failure;
                    }
                }
            }
        }

        let mut visitor = StringVisitor::default();
        event.record(&mut visitor);
        if let Some(report) = visitor.report {
            match report.parse() {
                Ok(report) => self.send(Message::Report(report)),
                Err(error) => {
                    eprintln!("Invalid KVP provisioning report: {error}");
                }
            }
            return;
        }

        if let Some(span) = ctx.lookup_current() {
            self.send(Message::Event {
                name: format_span_name(span.metadata()),
                payload: visitor.message,
                outcome: failed.then_some(Outcome::Failure),
            });
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id) {
            if let Some(state) = span.extensions().get::<SpanState>() {
                self.send(Message::Finish {
                    id: state.id.clone(),
                    name: format_span_name(span.metadata()),
                    outcome: state.outcome,
                    duration: state.started.elapsed(),
                });
            }
        }
    }
}

fn format_span_name(metadata: &tracing::Metadata<'_>) -> String {
    let target = metadata.target();
    let name = metadata.name();
    let module_path = target
        .strip_prefix("libazureinit::")
        .or_else(|| target.strip_prefix("azure_init::"))
        .unwrap_or(target);

    if module_path.is_empty() || module_path == target && target == name {
        name.to_string()
    } else {
        format!("{module_path}:{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{error::Error, health::encoded_success_report};
    use libazureinit_kvp::{
        Diagnostic, DiagnosticPayload, DiagnosticReader, Entry, KvpPool,
        PoolMode, PROVISIONING_REPORT_KEY,
    };
    use tempfile::TempDir;
    use tracing_subscriber::{layer::SubscriberExt, Registry};

    const VM_ID: &str = "00000000-0000-0000-0000-000000000abc";

    fn store(dir: &TempDir) -> KvpPoolStore {
        KvpPoolStore::new_in(KvpPool::Guest, dir.path(), PoolMode::Safe)
            .unwrap()
    }

    #[test]
    fn initialization_rejects_invalid_identity_and_missing_directory() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        assert!(Kvp::new(store.clone(), "invalid", CancellationToken::new())
            .is_err());
        assert!(!store.path().exists());
        let missing = KvpPoolStore::new_in(
            KvpPool::Guest,
            dir.path().join("missing"),
            PoolMode::Safe,
        )
        .unwrap();
        assert!(Kvp::new(missing, VM_ID, CancellationToken::new()).is_err());
    }

    #[tokio::test]
    async fn spans_and_events_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let shutdown = CancellationToken::new();
        let kvp = Kvp::new(store.clone(), VM_ID, shutdown.clone()).unwrap();
        let subscriber = Registry::default().with(kvp.tracing_layer);
        let payload = "quoted \"text\" | \u{1f600}".repeat(100);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("outside a span");
            let parent = tracing::info_span!("parent");
            let _parent = parent.enter();
            let child = tracing::info_span!("child", item = 7);
            {
                let _child = child.enter();
                tracing::info!(message = %payload);
                tracing::error!("child failure");
            }
            drop(child);
        });
        shutdown.cancel();
        kvp.writer.await.unwrap().unwrap();

        let entries = DiagnosticReader::new(store).entries().unwrap();
        assert_eq!(entries.len(), 6);
        let diagnostics: Vec<_> = entries
            .into_iter()
            .map(|entry| match entry {
                Entry::Diagnostic(diagnostic) => diagnostic,
                other => panic!("unexpected entry: {other:?}"),
            })
            .collect();
        assert!(matches!(&diagnostics[2], Diagnostic::Event(event)
            if event.payload == DiagnosticPayload::from(format!("message={payload}"))));
        for (start_index, finish_index) in [(0, 5), (1, 4)] {
            let Diagnostic::Start(start) = &diagnostics[start_index] else {
                panic!("missing start");
            };
            let Diagnostic::Finish(finish) = &diagnostics[finish_index] else {
                panic!("missing finish");
            };
            assert_eq!(start.key.event_id, finish.key.event_id);
            assert_eq!(start.key.name, finish.key.name);
            assert_eq!(start.key.vm_id.as_deref(), Some(VM_ID));
            assert_eq!(
                start.key.agent,
                concat!("azure-init/", env!("CARGO_PKG_VERSION"))
            );
            assert_eq!(start.payload, DiagnosticPayload::from(""));
            assert_eq!(finish.result, Outcome::Failure);
        }
    }

    #[tokio::test]
    async fn reports_replace_without_losing_quoted_values() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        store.insert("unrelated", "preserved").unwrap();
        let shutdown = CancellationToken::new();
        let kvp = Kvp::new(store.clone(), VM_ID, shutdown.clone()).unwrap();
        let subscriber = Registry::default().with(kvp.tracing_layer);
        let success = encoded_success_report(VM_ID, None);
        let failure = Error::LoadSshdConfig {
            details: "a \"quoted\" | failure".to_owned(),
        }
        .as_encoded_report(VM_ID);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(health_report = success.as_str());
            tracing::error!(health_report = %failure);
        });
        shutdown.cancel();
        kvp.writer.await.unwrap().unwrap();

        let records = store.dump().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            store.read("unrelated").unwrap().as_deref(),
            Some("preserved")
        );
        let stored = store.read(PROVISIONING_REPORT_KEY).unwrap().unwrap();
        assert_eq!(stored, failure);
        assert_eq!(
            stored.parse::<ProvisioningReport>().unwrap(),
            failure.parse::<ProvisioningReport>().unwrap()
        );
        assert!(DiagnosticReader::new(store)
            .entries()
            .unwrap()
            .iter()
            .any(|entry| matches!(entry, Entry::Report(_))));
    }

    #[tokio::test]
    async fn successful_span_and_filtered_events() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let shutdown = CancellationToken::new();
        let kvp = Kvp::new(store.clone(), VM_ID, shutdown.clone()).unwrap();
        let subscriber = Registry::default().with(
            kvp.tracing_layer
                .with_filter(tracing_subscriber::EnvFilter::new("info")),
        );
        tracing::subscriber::with_default(subscriber, || {
            let _span = tracing::info_span!("success").entered();
            tracing::debug!("filtered");
        });
        shutdown.cancel();
        kvp.writer.await.unwrap().unwrap();
        let entries = DiagnosticReader::new(store).entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            matches!(&entries[1], Entry::Diagnostic(Diagnostic::Finish(finish))
            if finish.result == Outcome::Success)
        );
    }

    #[tokio::test]
    async fn progress_does_not_replace_final_report() {
        use crate::{
            config::Config,
            health::report_in_progress,
            unittest::{get_http_response_payload, serve_requests},
        };
        use tracing::instrument::WithSubscriber;

        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let report = encoded_success_report(VM_ID, None);
        store.insert(PROVISIONING_REPORT_KEY, &report).unwrap();
        let shutdown = CancellationToken::new();
        let kvp = Kvp::new(store.clone(), VM_ID, shutdown.clone()).unwrap();
        let subscriber = Registry::default().with(kvp.tracing_layer);
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::default();
        config.wireserver.health_endpoint =
            format!("http://{}", listener.local_addr().unwrap());
        let server_shutdown = CancellationToken::new();
        let server = tokio::spawn(serve_requests(
            listener,
            get_http_response_payload(&reqwest::StatusCode::CREATED, ""),
            server_shutdown.clone(),
        ));
        report_in_progress(&config, VM_ID)
            .with_subscriber(subscriber)
            .await
            .unwrap();
        server_shutdown.cancel();
        server.await.unwrap();
        shutdown.cancel();
        kvp.writer.await.unwrap().unwrap();
        assert_eq!(
            store.read(PROVISIONING_REPORT_KEY).unwrap().as_deref(),
            Some(report.as_str())
        );
        assert!(DiagnosticReader::new(store).entries().unwrap().iter().any(
            |entry| matches!(entry, Entry::Diagnostic(Diagnostic::Event(event))
                if matches!(&event.payload, DiagnosticPayload::Text(text)
                    if text.contains("Provisioning is still in progress")))
        ));
    }

    #[tokio::test]
    async fn oversized_report_is_not_split_or_partially_written() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let report = encoded_success_report(VM_ID, None);
        store.insert(PROVISIONING_REPORT_KEY, &report).unwrap();
        let shutdown = CancellationToken::new();
        let kvp = Kvp::new(store.clone(), VM_ID, shutdown.clone()).unwrap();
        let subscriber = Registry::default().with(kvp.tracing_layer);
        let oversized =
            encoded_success_report(VM_ID, Some(("details", &"x".repeat(1023))));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(health_report = %oversized);
        });
        shutdown.cancel();
        assert!(kvp.writer.await.unwrap().is_err());
        assert_eq!(
            store.dump().unwrap(),
            vec![(PROVISIONING_REPORT_KEY.into(), report)]
        );
    }

    #[tokio::test]
    async fn writer_drains_and_returns_write_errors() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let shutdown = CancellationToken::new();
        let kvp = Kvp::new(store.clone(), VM_ID, shutdown.clone()).unwrap();
        let subscriber = Registry::default().with(kvp.tracing_layer);
        tracing::subscriber::with_default(subscriber, || {
            let _span = tracing::info_span!(target: "invalid|name", "invalid")
                .entered();
            tracing::info!("invalid diagnostic");
            tracing::info!(health_report = %encoded_success_report(VM_ID, None));
        });
        shutdown.cancel();
        assert!(kvp.writer.await.unwrap().is_err());
        assert!(store.read(PROVISIONING_REPORT_KEY).unwrap().is_some());
    }
}

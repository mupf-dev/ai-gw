// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Kosten von `RequestObserver::record()` im Hot Path (Ziel < 1 µs).
//!
//! `cargo bench -p aigw-observe --bench record`
//!
//! Gemessen wird nur der Aufruf; das Erzeugen des Eintrags (Strings, die der Proxy ohnehin
//! hat) liegt außerhalb der Messung. Ein Writer ohne Datenbank leert den Kanal parallel.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aigw_core::config::ObserveConfig;
use aigw_core::ir::Usage;
use aigw_core::observe::{PayloadMode, RequestObserver, RequestRecord};
use aigw_core::resource::new_id;
use aigw_core::types::ProviderKind;
use aigw_core::wire::WireFormat;
use aigw_observe::{
    GatewayMetrics, MetricsObserver, ObserveBuilder, ObserveHandle, ObserveMetrics, RequestLogger,
};
use tokio::sync::mpsc;

fn main() {
    divan::main();
}

fn record() -> RequestRecord {
    let mut r = RequestRecord::new(new_id().to_string(), WireFormat::OpenAiChat, 200);
    r.model_alias = Some("gpt-4o".into());
    r.model_group = Some("gpt-4o".into());
    r.deployment_id = Some(new_id());
    r.provider = Some(ProviderKind::OpenAi);
    r.provider_model = Some("gpt-4o-2024-08-06".into());
    r.api_key_id = Some(new_id());
    r.team_id = Some(new_id());
    r.organization_id = Some(new_id());
    r.usage = Usage {
        input_tokens: 1200,
        output_tokens: 300,
        ..Usage::default()
    };
    r.duration = Duration::from_millis(850);
    r.ttft = Some(Duration::from_millis(120));
    r.stream = true;
    r
}

struct Running {
    _rt: tokio::runtime::Runtime,
    handle: ObserveHandle,
}

/// Multi-Thread-Runtime mit Writer ohne Datenbank (nur Aufbereitung), großer Puffer.
fn running() -> &'static Running {
    static R: OnceLock<Running> = OnceLock::new();
    R.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.block_on(async {
            ObserveBuilder::new(ObserveConfig {
                queue_capacity: 1 << 20,
                ..ObserveConfig::default()
            })
            .start()
        });
        Running { _rt: rt, handle }
    })
}

/// Normalfall: Kanal hat Platz, Writer liest parallel.
#[divan::bench]
fn record_enqueue(b: divan::Bencher<'_, '_>) {
    let log = running().handle.logger();
    b.with_inputs(record)
        .bench_local_values(|r| log.record(divan::black_box(r)));
}

/// Überlauf: Kanal voll → verwerfen + Zähler (inkl. Freigabe des Eintrags).
#[divan::bench]
fn record_drop_when_full(b: divan::Bencher<'_, '_>) {
    let metrics = Arc::new(ObserveMetrics::default());
    let (tx, _rx) = mpsc::channel(1);
    let log = RequestLogger::new(tx, metrics, PayloadMode::Off);
    log.record(record());
    b.with_inputs(record)
        .bench_local_values(|r| log.record(divan::black_box(r)));
}

/// Nur die Prometheus-Metriken (Serie existiert bereits): Hash, Nachschlagen, Inkremente.
#[divan::bench]
fn metrics_observe(b: divan::Bencher<'_, '_>) {
    let m = GatewayMetrics::default();
    let template = record();
    m.observe(&template);
    b.with_inputs(|| {
        let mut r = record();
        r.deployment_id = template.deployment_id;
        r
    })
    .bench_local_refs(|r| m.observe(divan::black_box(r)));
}

/// Gesamtpfad im Betrieb: `request_started` + Metriken + Einreihen ins Request-Log
/// (`MetricsObserver` vor dem `RequestLogger`, wie `ObserveHandle::observer()`).
#[divan::bench]
fn record_with_metrics(b: divan::Bencher<'_, '_>) {
    let template = record();
    let obs = MetricsObserver::new(
        Arc::new(GatewayMetrics::default()),
        Arc::new(running().handle.logger()),
    );
    obs.request_started();
    obs.record(template.clone());
    b.with_inputs(|| {
        let mut r = record();
        r.deployment_id = template.deployment_id;
        r
    })
    .bench_local_values(|r| {
        obs.request_started();
        obs.record(divan::black_box(r));
    });
}

/// Zum Vergleich: Erzeugen eines Eintrags (außerhalb von `record()`).
#[divan::bench]
fn build_record() -> RequestRecord {
    record()
}

// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Kosten bei abgeschaltetem Tracing: Mit dem Subscriber-Aufbau des Binarys (Log-Layer mit
//! `aigw::otel=off`, Otel-Layer inaktiv) sind die Trace-Callsites ganz abgeschaltet
//! (`Span::none()`); nach `activate()` werden sie aufgezeichnet.
//!
//! Eigene Testdatei = eigener Prozess: Das Callsite-Interesse ist global und würde sonst von
//! parallel laufenden Tests mit aktivem Layer beeinflusst.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use aigw_core::config::{OtlpConfig, OtlpTracesConfig};
use aigw_core::observe::TRACE_TARGET;
use aigw_observe::otel::{OtlpMetrics, layer};
use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
use tracing_subscriber::{EnvFilter, Registry};

fn span() -> tracing::Span {
    tracing::info_span!(target: TRACE_TARGET, "request", otel.name = "chat")
}

#[tokio::test]
async fn spans_disabled_until_activated() {
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::sink)
        .with_filter(EnvFilter::new("info,aigw::otel=off"));
    let (otel, activator) = layer();
    let subscriber = Registry::default().with(fmt).with(otel);
    let _guard = tracing::subscriber::set_default(subscriber);

    assert!(span().is_disabled());
    assert!(aigw_core::observe::current_traceparent().is_none());

    let cfg = OtlpConfig {
        endpoint: "http://127.0.0.1:9".into(),
        traces: OtlpTracesConfig {
            enabled: true,
            ..OtlpTracesConfig::default()
        },
        ..OtlpConfig::default()
    };
    let exporter = activator
        .activate(&cfg, Arc::new(OtlpMetrics::default()))
        .unwrap();
    let s = span();
    assert!(!s.is_disabled());
    let tp = s.in_scope(aigw_core::observe::current_traceparent).unwrap();
    assert!(tp.starts_with("00-") && tp.ends_with("-01"), "{tp}");
    drop(s);
    // Der Collector ist nicht erreichbar: Export schlägt fehl, Shutdown endet trotzdem.
    assert!(exporter.shutdown(Duration::from_secs(10)).await);
}

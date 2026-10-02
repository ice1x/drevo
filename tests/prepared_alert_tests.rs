//! Alerting on abandoned prepared transactions — #556 slice 2 (RFC §2.4, D2).
//!
//! A prepared transaction is never resolved automatically, but while it
//! exists every write is refused, so an abandoned one must be loud: once it
//! is older than `DREVO_PREPARED_TX_WARN_SECS` (default 60, `0` = off) the
//! server logs one ERROR per transaction, which lands in the Web UI's problem
//! feed (#552). A transaction is reported once; if it is resolved and a new
//! one reuses the gid, that one is reported afresh.

#![cfg(feature = "http")]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use drevo::native::PreparedInfo;
use drevo::problems::{report_stale_prepared, ProblemLayer, ProblemLog};
use tracing_subscriber::layer::SubscriberExt;

fn info(gid: &str, prepared_at_ms: i64) -> PreparedInfo {
    PreparedInfo {
        gid: gid.to_string(),
        prepared_at_ms,
        op_count: 2,
    }
}

#[test]
fn stale_prepared_transactions_are_reported_once() {
    let log = Arc::new(ProblemLog::new(16));
    let subscriber = tracing_subscriber::registry().with(ProblemLayer::new(Arc::clone(&log)));
    let mut reported = HashSet::new();
    let warn_after = Duration::from_secs(60);
    let now = 1_000_000_000;

    tracing::subscriber::with_default(subscriber, || {
        let fresh = info("fresh", now - 10_000);
        let stale = info("stale", now - 120_000);
        let first = report_stale_prepared(
            "drevo",
            &[fresh.clone(), stale.clone()],
            now,
            warn_after,
            &mut reported,
        );
        assert_eq!(first, vec!["stale".to_string()]);
        // The next tick does not repeat it.
        let second = report_stale_prepared(
            "drevo",
            &[fresh, stale],
            now + 10_000,
            warn_after,
            &mut reported,
        );
        assert!(second.is_empty());
        // Resolved, then a new transaction reuses the gid: reported again.
        report_stale_prepared("drevo", &[], now + 20_000, warn_after, &mut reported);
        let again = report_stale_prepared(
            "drevo",
            &[info("stale", now)],
            now + 200_000,
            warn_after,
            &mut reported,
        );
        assert_eq!(again, vec!["stale".to_string()]);
    });

    let (problems, _) = log.since(0);
    assert_eq!(problems.len(), 2);
    let p = &problems[0];
    assert_eq!(p.level, "ERROR");
    assert_eq!(p.target, "drevo::tx");
    assert_eq!(p.fields["gid"], "stale");
    assert_eq!(p.fields["database"], "drevo");
    assert_eq!(p.fields["age_secs"], "120");
    assert!(p.message.contains("prepared transaction"), "{}", p.message);
}

#[test]
fn warn_threshold_env_var_parses() {
    let cfg = |v: Option<&str>| {
        drevo::server::Config::from_env(|key| match key {
            "DREVO_PREPARED_TX_WARN_SECS" => v.map(str::to_string),
            _ => None,
        })
    };
    assert_eq!(
        cfg(None).unwrap().prepared_tx_warn,
        Some(Duration::from_secs(60))
    );
    assert_eq!(cfg(Some("0")).unwrap().prepared_tx_warn, None);
    assert_eq!(
        cfg(Some("5")).unwrap().prepared_tx_warn,
        Some(Duration::from_secs(5))
    );
    assert!(cfg(Some("soon")).is_err());
}

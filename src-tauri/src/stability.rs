//! Repeated, independent TCP connections. A failed connection is not an ICMP
//! packet-loss observation. Only completed attempts enter the statistics.

use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, time::Duration};
use tokio::{net::TcpStream, time::MissedTickBehavior};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TcpStability {
    pub requested: usize,
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub over_limit_count: usize,
    pub delay_limit_ms: f64,
    pub failure_percent: Option<f64>,
    pub average_ms: Option<f64>,
    pub median_ms: Option<f64>,
    pub min_ms: Option<f64>,
    pub max_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    /// Mean absolute difference between consecutive successful attempts.
    /// A failed attempt breaks the pair; no adjacent pair yields None.
    pub jitter_ms: Option<f64>,
    /// None means a completed connection failure, not an unexecuted attempt.
    pub probes_ms: Vec<Option<f64>>,
    pub completed: bool,
    /// A partial measurement stopped after its rejection rule was met.
    /// Consumers must rederive this proof from the raw probes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub early_rejection: Option<String>,
}

pub fn summarize(
    requested: usize,
    delay_limit_ms: f64,
    probes_ms: Vec<Option<f64>>,
) -> TcpStability {
    let probes_ms: Vec<_> = probes_ms
        .into_iter()
        .map(|sample| sample.filter(|value| value.is_finite() && *value >= 0.0))
        .collect();
    let attempted = probes_ms.len();
    let mut successful: Vec<_> = probes_ms.iter().flatten().copied().collect();
    successful.sort_by(f64::total_cmp);
    let succeeded = successful.len();
    let failed = attempted - succeeded;
    let average_ms =
        (!successful.is_empty()).then(|| successful.iter().sum::<f64>() / succeeded as f64);
    let median_ms = (!successful.is_empty()).then(|| {
        let middle = succeeded / 2;
        if succeeded % 2 == 0 {
            (successful[middle - 1] + successful[middle]) / 2.0
        } else {
            successful[middle]
        }
    });
    // Nearest-rank percentile: the ceil(0.95 * n)-th successful observation.
    let p95_ms = (!successful.is_empty()).then(|| successful[(95 * succeeded).div_ceil(100) - 1]);
    let adjacent_differences: Vec<_> = probes_ms
        .windows(2)
        .filter_map(|pair| match (pair[0], pair[1]) {
            (Some(a), Some(b)) => Some((b - a).abs()),
            _ => None,
        })
        .collect();
    let jitter_ms = (!adjacent_differences.is_empty())
        .then(|| adjacent_differences.iter().sum::<f64>() / adjacent_differences.len() as f64);
    TcpStability {
        requested,
        attempted,
        succeeded,
        failed,
        over_limit_count: successful
            .iter()
            .filter(|value| **value > delay_limit_ms)
            .count(),
        delay_limit_ms,
        failure_percent: (attempted > 0).then(|| failed as f64 * 100.0 / attempted as f64),
        average_ms,
        median_ms,
        min_ms: successful.first().copied(),
        max_ms: successful.last().copied(),
        p95_ms,
        jitter_ms,
        probes_ms,
        completed: requested > 0 && attempted == requested,
        early_rejection: None,
    }
}

/// Derive the policy/display value from actual successful observations, never
/// from cached aggregate fields. Failed probes enter failure rate separately.
pub fn latency_metric_ms(probes: &[Option<f64>], metric: &str) -> Option<f64> {
    if probes
        .iter()
        .flatten()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return None;
    }
    let mut successful: Vec<_> = probes.iter().flatten().copied().collect();
    if successful.is_empty() {
        return None;
    }
    successful.sort_by(f64::total_cmp);
    match metric {
        "average" => Some(successful.iter().sum::<f64>() / successful.len() as f64),
        "p95" => Some(successful[(95 * successful.len()).div_ceil(100) - 1]),
        "max" => successful.last().copied(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}

/// Prove failure against the configured *final* number of attempts, rather
/// than a provisional percentile/failure rate. Every unexecuted attempt is
/// optimistically assumed to connect instantly. If that best possible outcome
/// still fails, continuing cannot produce a passing candidate.
pub fn impossible_to_pass(
    probes: &[Option<f64>],
    requested: usize,
    max_tcp_ms: f64,
    max_failure_percent: f64,
    latency_metric: &str,
) -> Option<String> {
    if !(1..=1000).contains(&requested)
        || probes.len() > requested
        || !max_tcp_ms.is_finite()
        || max_tcp_ms <= 0.0
        || !max_failure_percent.is_finite()
        || !(0.0..=100.0).contains(&max_failure_percent)
        || !matches!(latency_metric, "average" | "p95" | "max")
        || probes
            .iter()
            .flatten()
            .any(|value| !value.is_finite() || *value < 0.0)
    {
        return None;
    }

    let failed = probes.iter().filter(|probe| probe.is_none()).count();
    let best_failure_percent = failed as f64 * 100.0 / requested as f64;
    if best_failure_percent > max_failure_percent {
        return Some(format!(
            "TCP 稳定性提前淘汰：已连接失败 {failed} 次，即使剩余探测全部成功，最终失败率仍为 {best_failure_percent:.1}%，超过上限 {max_failure_percent}%"
        ));
    }

    // Additional failures can only lower the final successful-sample count,
    // making each latency policy at least as strict. All remaining successes
    // at 0 ms therefore give the most optimistic final bound.
    let max_successful = requested - failed;
    if max_successful == 0 {
        return Some("TCP 稳定性提前淘汰：全部连接失败，没有可用延迟样本".into());
    }
    let slow = probes
        .iter()
        .flatten()
        .filter(|value| **value > max_tcp_ms)
        .count();
    match latency_metric {
        "p95" => {
            // Nearest rank: n - ceil(0.95*n) = floor(0.05*n).
            let max_slow = max_successful * 5 / 100;
            (slow > max_slow).then(|| {
                format!(
                    "TCP 稳定性提前淘汰：已有 {slow} 个样本超过 {max_tcp_ms} ms，最终 P95 达标最多允许 {max_slow} 个；剩余探测全部成功也无法达标"
                )
            })
        }
        "max" => (slow > 0).then(|| {
            format!("TCP 稳定性提前淘汰：已出现超过 {max_tcp_ms} ms 的样本，最终最大延迟无法达标")
        }),
        "average" => {
            let lowest_average: f64 = probes
                .iter()
                .flatten()
                .map(|value| value / max_successful as f64)
                .sum();
            // Be conservative around floating-point equality: never reject a
            // boundary candidate because of accumulated rounding error.
            let rounding_margin = f64::EPSILON * requested as f64 * max_tcp_ms.max(1.0) * 4.0;
            (lowest_average > max_tcp_ms + rounding_margin).then(|| {
                format!(
                    "TCP 稳定性提前淘汰：即使剩余探测均为 0 ms，最终平均延迟仍至少为 {lowest_average:.1} ms，超过上限 {max_tcp_ms} ms"
                )
            })
        }
        _ => unreachable!("validated latency policy"),
    }
}

/// The current scan policy discards a candidate on its second cumulative
/// connection failure, independently of the configured attempt count/rate.
/// Derive the count from raw observations rather than cached statistics.
pub fn failure_count_exclusion(probes: &[Option<f64>]) -> Option<String> {
    if probes
        .iter()
        .flatten()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return None;
    }
    let failed = probes.iter().filter(|probe| probe.is_none()).count();
    (failed >= 2).then(|| format!("累计 {failed} 次连接失败，达到 2 次淘汰阈值"))
}

#[derive(Clone, Copy)]
enum ProbePolicy<'a> {
    FullLength,
    FinalLimits(f64, &'a str),
    TwoFailures,
}

/// Send at most one connection at a time to this candidate. Cadence controls
/// start times; a slow probe never causes overlapping catch-up connections.
/// This full-length form also supports the explicit socket-comparison example.
pub async fn probe_tcp<F: FnMut(&TcpStability) + Send>(
    addr: SocketAddr,
    attempts: usize,
    interval: Duration,
    timeout: Duration,
    max_tcp_ms: f64,
    cancellation: CancellationToken,
    on_update: F,
) -> TcpStability {
    probe_tcp_inner(
        addr,
        attempts,
        interval,
        timeout,
        max_tcp_ms,
        ProbePolicy::FullLength,
        cancellation,
        on_update,
    )
    .await
}

/// Legacy policy-aware probing remains available for existing comparisons and
/// historical rejection proofs. New scans use `probe_tcp_with_failure_limit`.
pub async fn probe_tcp_with_policy<F: FnMut(&TcpStability) + Send>(
    addr: SocketAddr,
    attempts: usize,
    interval: Duration,
    timeout: Duration,
    max_tcp_ms: f64,
    max_failure_percent: f64,
    latency_metric: &str,
    cancellation: CancellationToken,
    on_update: F,
) -> TcpStability {
    probe_tcp_inner(
        addr,
        attempts,
        interval,
        timeout,
        max_tcp_ms,
        ProbePolicy::FinalLimits(max_failure_percent, latency_metric),
        cancellation,
        on_update,
    )
    .await
}

/// Finish the requested probes unless cancelled or two real connections fail.
/// Latency and configured failure-rate limits are evaluated after completion;
/// a slow partial measurement is never enough to stop probing a candidate.
pub async fn probe_tcp_with_failure_limit<F: FnMut(&TcpStability) + Send>(
    addr: SocketAddr,
    attempts: usize,
    interval: Duration,
    timeout: Duration,
    max_tcp_ms: f64,
    cancellation: CancellationToken,
    on_update: F,
) -> TcpStability {
    probe_tcp_inner(
        addr,
        attempts,
        interval,
        timeout,
        max_tcp_ms,
        ProbePolicy::TwoFailures,
        cancellation,
        on_update,
    )
    .await
}

async fn probe_tcp_inner<F: FnMut(&TcpStability) + Send>(
    addr: SocketAddr,
    attempts: usize,
    interval: Duration,
    timeout: Duration,
    max_tcp_ms: f64,
    policy: ProbePolicy<'_>,
    cancellation: CancellationToken,
    mut on_update: F,
) -> TcpStability {
    let mut probes_ms = Vec::with_capacity(attempts.min(1000));
    let initial = summarize(attempts, max_tcp_ms, Vec::new());
    on_update(&initial);
    if !(1..=1000).contains(&attempts)
        || interval.is_zero()
        || timeout.is_zero()
        || !max_tcp_ms.is_finite()
        || max_tcp_ms <= 0.0
    {
        return initial;
    }
    let mut cadence = tokio::time::interval(interval);
    cadence.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut early_rejection = None;
    for _ in 0..attempts {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => break,
            _ = cadence.tick() => {}
        }
        let started = std::time::Instant::now();
        let sample = tokio::select! {
            biased;
            _ = cancellation.cancelled() => break,
            connected = tokio::time::timeout(timeout, TcpStream::connect(addr)) => {
                match connected {
                    Ok(Ok(stream)) => {
                        let ms = started.elapsed().as_secs_f64() * 1000.0;
                        drop(stream);
                        Some(ms)
                    }
                    _ => None,
                }
            }
        };
        probes_ms.push(sample);
        let mut update = summarize(attempts, max_tcp_ms, probes_ms.clone());
        if probes_ms.len() < attempts {
            early_rejection = match policy {
                ProbePolicy::FullLength => None,
                ProbePolicy::FinalLimits(max_failure_percent, metric) => impossible_to_pass(
                    &probes_ms,
                    attempts,
                    max_tcp_ms,
                    max_failure_percent,
                    metric,
                ),
                ProbePolicy::TwoFailures => failure_count_exclusion(&probes_ms)
                    .map(|reason| format!("TCP 稳定性提前淘汰：{reason}")),
            };
        }
        update.early_rejection = early_rejection.clone();
        on_update(&update);
        if early_rejection.is_some() {
            break;
        }
    }
    let mut result = summarize(attempts, max_tcp_ms, probes_ms);
    result.early_rejection = early_rejection;
    on_update(&result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_use_completed_attempts_and_do_not_bridge_failed_probes() {
        let stats = summarize(
            100,
            200.0,
            vec![Some(10.0), Some(20.0), None, Some(40.0), Some(80.0)],
        );
        assert_eq!((stats.attempted, stats.succeeded, stats.failed), (5, 4, 1));
        assert_eq!(stats.failure_percent, Some(20.0));
        assert_eq!(stats.average_ms, Some(37.5));
        assert_eq!(stats.median_ms, Some(30.0));
        assert_eq!(stats.p95_ms, Some(80.0));
        assert_eq!(stats.jitter_ms, Some(25.0));
        assert!(!stats.completed);
    }

    #[test]
    fn empty_or_failed_measurements_do_not_invent_zero_latency() {
        let empty = summarize(100, 200.0, vec![]);
        assert_eq!(empty.failure_percent, None);
        assert_eq!(empty.average_ms, None);
        assert_eq!(empty.p95_ms, None);
        let failed = summarize(100, 200.0, vec![None; 100]);
        assert!(failed.completed);
        assert_eq!(failed.failure_percent, Some(100.0));
        assert_eq!(failed.min_ms, None);
        assert_eq!(failed.jitter_ms, None);
    }

    #[test]
    fn nearest_rank_p95_and_each_observed_tcp_limit_are_preserved() {
        let mut samples = vec![Some(50.0); 90];
        samples.extend(vec![Some(150.0); 9]);
        samples.push(Some(200.001));
        let stats = summarize(100, 200.0, samples);
        assert!(stats.completed);
        assert_eq!(stats.p95_ms, Some(150.0));
        assert_eq!(stats.over_limit_count, 1);
        assert_eq!(stats.max_ms, Some(200.001));
        assert!(summarize(1, 200.0, vec![Some(200.0)]).over_limit_count == 0);
    }

    #[test]
    fn selected_latency_is_derived_from_actual_samples_and_not_failed_attempt_durations() {
        let mut samples = vec![Some(150.0); 95];
        samples.extend(vec![Some(1000.0); 4]);
        samples.push(None);
        assert_eq!(latency_metric_ms(&samples, "p95"), Some(150.0));
        assert_eq!(latency_metric_ms(&samples, "max"), Some(1000.0));
        assert!(latency_metric_ms(&samples, "average").unwrap() > 150.0);
        assert_eq!(latency_metric_ms(&samples, "median"), None);
        assert_eq!(latency_metric_ms(&[None], "p95"), None);
        assert_eq!(latency_metric_ms(&[Some(f64::NAN)], "p95"), None);
    }

    #[test]
    fn failure_rejection_uses_the_final_denominator_and_stops_at_the_second_failure() {
        assert!(impossible_to_pass(&[None], 100, 230.0, 1.0, "p95").is_none());
        let reason = impossible_to_pass(&[None, None], 100, 230.0, 1.0, "p95").unwrap();
        assert!(reason.contains("已连接失败 2 次"));
        assert!(reason.contains("2.0%"));
        assert!(impossible_to_pass(&[None], 100, 230.0, 0.0, "p95").is_some());
    }

    #[test]
    fn fixed_failure_limit_counts_cumulative_raw_failures() {
        assert!(failure_count_exclusion(&[Some(5000.0), None]).is_none());
        let reason = failure_count_exclusion(&[None, Some(20.0), None]).unwrap();
        assert!(reason.contains("累计 2 次连接失败"));
        assert!(failure_count_exclusion(&[None, None, Some(f64::NAN)]).is_none());
        assert!(failure_count_exclusion(&[None, None, Some(-1.0)]).is_none());
    }

    #[test]
    fn p95_rejection_counts_only_samples_that_cannot_be_diluted_by_remaining_successes() {
        let mut probes = vec![Some(230.001); 5];
        // An observed partial P95 above the target is not itself a rejection.
        assert!(impossible_to_pass(&probes, 100, 230.0, 1.0, "p95").is_none());
        probes.push(Some(230.001));
        assert!(impossible_to_pass(&probes, 100, 230.0, 1.0, "p95").is_some());

        let mut one_failure = vec![None];
        one_failure.extend(vec![Some(231.0); 4]);
        assert!(impossible_to_pass(&one_failure, 100, 230.0, 1.0, "p95").is_none());
        one_failure.push(Some(231.0));
        assert!(impossible_to_pass(&one_failure, 100, 230.0, 1.0, "p95").is_some());

        let at_target = vec![Some(230.0); 100];
        assert!(impossible_to_pass(&at_target, 100, 230.0, 1.0, "p95").is_none());
    }

    #[test]
    fn average_and_max_policies_use_optimistic_final_bounds() {
        assert!(impossible_to_pass(&[Some(231.0)], 100, 230.0, 1.0, "max").is_some());
        assert!(impossible_to_pass(&[Some(230.0)], 100, 230.0, 1.0, "max").is_none());
        assert!(impossible_to_pass(&[Some(23000.0)], 100, 230.0, 1.0, "average").is_none());
        assert!(impossible_to_pass(&[Some(23000.1)], 100, 230.0, 1.0, "average").is_some());
        assert!(impossible_to_pass(&[None, Some(23000.0)], 100, 230.0, 1.0, "average").is_some());
    }

    #[test]
    fn invalid_samples_or_policy_cannot_be_used_as_an_early_failure_proof() {
        for probes in [
            vec![Some(f64::NAN)],
            vec![Some(f64::INFINITY)],
            vec![Some(-1.0)],
        ] {
            assert!(impossible_to_pass(&probes, 100, 230.0, 1.0, "p95").is_none());
        }
        for requested in [0, 1001] {
            assert!(impossible_to_pass(&[None; 3], requested, 230.0, 1.0, "p95").is_none());
        }
        assert!(impossible_to_pass(&[None; 3], 2, 230.0, 1.0, "p95").is_none());
        for limit in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(impossible_to_pass(&[None; 3], 100, limit, 1.0, "p95").is_none());
        }
        for failure in [-1.0, 101.0, f64::NAN, f64::INFINITY] {
            assert!(impossible_to_pass(&[None; 3], 100, 230.0, failure, "p95").is_none());
        }
        assert!(impossible_to_pass(&[None; 3], 100, 230.0, 1.0, "median").is_none());
    }

    #[test]
    fn every_early_proof_rejects_all_possible_grid_completions_for_each_policy() {
        // Exhaustive short sequences exercise percentile nearest-rank rounding,
        // failures and boundary values; a rejected prefix must have no passing
        // completion even when every later connection is instant and reliable.
        let values = [None, Some(0.0), Some(230.0), Some(231.0)];
        for requested in 1..=7 {
            for encoded in 0..4_usize.pow(requested as u32) {
                let mut code = encoded;
                let full: Vec<_> = (0..requested)
                    .map(|_| {
                        let value = values[code % 4];
                        code /= 4;
                        value
                    })
                    .collect();
                let failed = full.iter().filter(|probe| probe.is_none()).count();
                for metric in ["average", "p95", "max"] {
                    for max_failure in [0.0, 25.0, 100.0] {
                        let passes = failed as f64 * 100.0 / requested as f64 <= max_failure
                            && latency_metric_ms(&full, metric).is_some_and(|ms| ms <= 230.0);
                        if !passes {
                            continue;
                        }
                        for prefix in 0..=requested {
                            assert!(
                                impossible_to_pass(&full[..prefix], requested, 230.0, max_failure, metric).is_none(),
                                "false rejection: metric={metric}, max_failure={max_failure}, prefix={:?}, completion={full:?}",
                                &full[..prefix]
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn early_rejection_is_optional_in_old_serialized_measurements() {
        let old = serde_json::to_value(summarize(100, 230.0, vec![Some(20.0)])).unwrap();
        assert!(old.get("earlyRejection").is_none());
        let recovered: TcpStability = serde_json::from_value(old).unwrap();
        assert!(recovered.early_rejection.is_none());
    }

    #[tokio::test]
    async fn actual_tcp_worker_stops_after_two_real_failed_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let mut updates = Vec::new();
        let stats = probe_tcp_with_policy(
            addr,
            100,
            Duration::from_millis(1),
            Duration::from_secs(1),
            230.0,
            1.0,
            "p95",
            CancellationToken::new(),
            |update| updates.push(update.clone()),
        )
        .await;
        assert_eq!((stats.attempted, stats.failed), (2, 2));
        assert_eq!(stats.probes_ms, vec![None, None]);
        assert!(!stats.completed);
        assert!(stats.early_rejection.is_some());
        assert_eq!(updates.iter().map(|update| update.attempted).max(), Some(2));
        assert!(updates
            .iter()
            .any(|update| update.early_rejection.is_some()));
    }

    #[tokio::test]
    async fn current_tcp_worker_stops_at_two_failures_even_with_a_long_probe_plan() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let stats = probe_tcp_with_failure_limit(
            addr,
            1000,
            Duration::from_millis(1),
            Duration::from_secs(1),
            230.0,
            CancellationToken::new(),
            |_| {},
        )
        .await;
        assert_eq!((stats.attempted, stats.failed), (2, 2));
        assert_eq!(stats.probes_ms, vec![None, None]);
        assert!(!stats.completed);
        assert!(stats.early_rejection.unwrap().contains("累计 2 次连接失败"));
    }

    #[tokio::test]
    async fn current_tcp_worker_finishes_all_probes_even_when_every_latency_is_over_limit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..10 {
                let (stream, _) = listener.accept().await.unwrap();
                drop(stream);
            }
        });
        let stats = probe_tcp_with_failure_limit(
            addr,
            10,
            Duration::from_millis(1),
            Duration::from_secs(1),
            f64::MIN_POSITIVE,
            CancellationToken::new(),
            |_| {},
        )
        .await;
        assert_eq!((stats.attempted, stats.succeeded), (10, 10));
        assert_eq!(stats.over_limit_count, 10);
        assert!(stats.completed);
        assert!(stats.early_rejection.is_none());
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn makes_one_hundred_real_independent_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..100 {
                let (stream, _) = listener.accept().await.unwrap();
                drop(stream);
            }
            100
        });
        let mut progress = Vec::new();
        let stats = probe_tcp_with_policy(
            addr,
            100,
            Duration::from_millis(1),
            Duration::from_secs(1),
            200.0,
            1.0,
            "p95",
            CancellationToken::new(),
            |update| progress.push(update.attempted),
        )
        .await;
        assert_eq!(server.await.unwrap(), 100);
        assert_eq!(
            (stats.requested, stats.attempted, stats.succeeded),
            (100, 100, 100)
        );
        assert!(stats.completed);
        assert!(stats.early_rejection.is_none());
        assert!(progress.contains(&37));
    }

    #[tokio::test]
    async fn cancelling_keeps_only_completed_attempts_without_counting_unsent_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token = CancellationToken::new();
        let cancel = token.clone();
        let stats = probe_tcp(
            listener.local_addr().unwrap(),
            100,
            Duration::from_millis(1),
            Duration::from_secs(1),
            200.0,
            token,
            move |update| {
                if update.attempted == 3 {
                    cancel.cancel();
                }
            },
        )
        .await;
        assert_eq!((stats.attempted, stats.succeeded, stats.failed), (3, 3, 0));
        assert_eq!(stats.failure_percent, Some(0.0));
        assert!(!stats.completed);
        assert!(stats.early_rejection.is_none());
    }

    #[tokio::test]
    async fn cancelled_before_start_opens_no_connection() {
        let token = CancellationToken::new();
        token.cancel();
        let result = probe_tcp(
            "127.0.0.1:9".parse().unwrap(),
            100,
            Duration::from_millis(1),
            Duration::from_secs(1),
            200.0,
            token,
            |_| {},
        )
        .await;
        assert_eq!(result.attempted, 0);
        assert!(!result.completed);
        assert!(result.early_rejection.is_none());
    }
}

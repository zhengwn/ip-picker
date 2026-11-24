//! Real, bounded measurements. Every HTTPS request uses its own HTTP/1 client,
//! binds the URL host to exactly one candidate, validates TLS normally, and
//! checks the connected peer. Results describe the current network and target.

use crate::model::{
    parse_custom_candidates, pop, sort_result_indices, CustomCandidates, ExclusionCode, IpResult,
    ScanCommandError, ScanConfig, ScanProgress, StabilityBatch,
};
use crate::stability::{measure_tcp_connect, TcpConnectFailure, TcpConnectFailureKind};
use futures_util::{stream, StreamExt};
use ipnet::Ipv4Net;
use rand::Rng;
use reqwest::{header, redirect::Policy, Client, Response};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use url::Url;

const CLOUDFLARE_IPV4: &[&str] = &[
    // https://www.cloudflare.com/ips-v4 (checked 2026-10-23).
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
];
const MAX_TRACE_BYTES: usize = 16 * 1024;
const MIN_SAMPLE_BYTES: u64 = 64 * 1024;
const EVENT_INTERVAL: Duration = Duration::from_millis(175);
const DOWNLOAD_CANDIDATES_PER_REGION: usize = 10;

#[derive(Debug, Clone)]
pub struct EngineUpdate {
    pub progress: ScanProgress,
    pub results: Option<Arc<Vec<IpResult>>>,
    pub changed_rows: BTreeMap<usize, IpResult>,
}

impl EngineUpdate {
    pub(crate) fn coalesce(&mut self, newer: Self) {
        self.progress = newer.progress;
        if newer.results.is_some() {
            self.results = newer.results;
            self.changed_rows = newer.changed_rows;
        } else {
            self.changed_rows.extend(newer.changed_rows);
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineOutcome {
    pub progress: ScanProgress,
    pub results: Vec<IpResult>,
    pub cancelled: bool,
}

pub type UpdateCallback = Arc<dyn Fn(EngineUpdate) + Send + Sync>;

/// Validated configuration and its already sampled candidates. Keep both fields
/// private so execution cannot receive an unprepared or mismatched candidate set.
#[derive(Debug)]
pub(crate) struct PreparedScan {
    config: ScanConfig,
    ips: Vec<Ipv4Addr>,
    scope: ScanScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanScope {
    Regional,
    Targeted,
    DownloadOnly,
}

struct State {
    progress: ScanProgress,
    results: Vec<IpResult>,
    last_event: Instant,
}

struct ProgressUpdates<'a> {
    callback: &'a UpdateCallback,
    changed: BTreeSet<usize>,
    completed: usize,
    downloaded_bytes: u64,
    message: String,
}

impl<'a> ProgressUpdates<'a> {
    fn new(state: &State, callback: &'a UpdateCallback) -> Self {
        Self {
            callback,
            changed: BTreeSet::new(),
            completed: state.progress.completed,
            downloaded_bytes: state.progress.downloaded_bytes,
            message: state.progress.message.clone(),
        }
    }

    fn emit(&mut self, state: &mut State, force: bool) {
        if !force && state.last_event.elapsed() < EVENT_INTERVAL {
            return;
        }
        if self.changed.is_empty()
            && self.completed == state.progress.completed
            && self.downloaded_bytes == state.progress.downloaded_bytes
            && self.message == state.progress.message
        {
            return;
        }
        self.completed = state.progress.completed;
        self.downloaded_bytes = state.progress.downloaded_bytes;
        self.message.clone_from(&state.progress.message);
        let changed_rows = std::mem::take(&mut self.changed)
            .into_iter()
            .map(|index| (index, state.results[index].clone()))
            .collect();
        state.send_update(self.callback, None, changed_rows);
    }
}

impl State {
    fn emit(&mut self, callback: &UpdateCallback, force: bool) {
        if force || self.last_event.elapsed() >= EVENT_INTERVAL {
            self.send_update(
                callback,
                Some(Arc::new(self.results.clone())),
                BTreeMap::new(),
            );
        }
    }

    fn send_update(
        &mut self,
        callback: &UpdateCallback,
        results: Option<Arc<Vec<IpResult>>>,
        changed_rows: BTreeMap<usize, IpResult>,
    ) {
        callback(EngineUpdate {
            progress: self.progress.clone(),
            results,
            changed_rows,
        });
        self.last_event = Instant::now();
    }

    fn stage(&mut self, stage: &str, total: usize, message: String, callback: &UpdateCallback) {
        if stage != "stabilizing" {
            self.progress.stability_batch = None;
        }
        if !matches!(stage, "verifying" | "stabilizing" | "downloading") {
            self.progress.precision_batch = None;
        }
        self.progress.stage = stage.into();
        self.progress.completed = 0;
        self.progress.total = total;
        self.progress.message = message;
        self.emit(callback, true);
    }

    fn begin_stability_batch(
        &mut self,
        number: usize,
        candidates: &[usize],
        attempts: usize,
        callback: &UpdateCallback,
    ) {
        self.progress.stability_batch = Some(Arc::new(StabilityBatch {
            number,
            candidate_ips: candidates
                .iter()
                .map(|index| self.results[*index].ip.clone())
                .collect(),
        }));
        self.stage(
            "stabilizing",
            candidates.len() * attempts,
            format!("第 {number} 批 TCP 稳定性：每个候选 {attempts} 次独立连接"),
            callback,
        );
    }

    fn cancel(
        mut self,
        config: &ScanConfig,
        scope: ScanScope,
        callback: &UpdateCallback,
    ) -> EngineOutcome {
        for item in &mut self.results {
            item.mark_cancelled(config);
        }
        if scope != ScanScope::DownloadOnly {
            rank_results(&mut self.results, config, false);
        }
        self.progress.stage = "cancelled".into();
        self.progress.stability_batch = None;
        self.progress.precision_batch = None;
        self.progress.message = "测试已停止；保留已完成的测量，未完成项不参与推荐".into();
        self.emit(callback, true);
        EngineOutcome {
            progress: self.progress,
            results: self.results,
            cancelled: true,
        }
    }
}

/// Run a full scan. Cancellation returns a partial outcome, not an error. Invalid
/// input returns Err before opening any network connection.
pub async fn run_scan(
    config: ScanConfig,
    cancellation: CancellationToken,
    callback: UpdateCallback,
) -> Result<EngineOutcome, String> {
    let prepared = prepare_scan(config).map_err(ScanCommandError::into_message)?;
    run_prepared_scan(prepared, cancellation, callback).await
}

/// Finish all fallible input preparation before replacing a saved scan. This
/// performs no network measurements and samples random candidates only once.
pub(crate) fn prepare_scan(config: ScanConfig) -> Result<PreparedScan, ScanCommandError> {
    let ips = match config.validate_with_candidates()? {
        Some(candidates) => sample_custom_candidates(candidates, config.max_candidates)?,
        None => sample_cloudflare(config.max_candidates)?,
    };
    Ok(PreparedScan {
        config,
        ips,
        scope: ScanScope::Regional,
    })
}

pub(crate) fn prepare_ip_retest(
    config: ScanConfig,
    ip: Ipv4Addr,
) -> Result<PreparedScan, ScanCommandError> {
    config.validate_with_candidates()?;
    Ok(PreparedScan {
        config,
        ips: vec![ip],
        scope: ScanScope::Targeted,
    })
}

/// Execute exactly the candidates that were accepted before the scan was saved.
pub(crate) async fn run_prepared_scan(
    prepared: PreparedScan,
    cancellation: CancellationToken,
    callback: UpdateCallback,
) -> Result<EngineOutcome, String> {
    let PreparedScan { config, ips, scope } = prepared;
    run_candidates(config, ips, scope, cancellation, callback).await
}

/// Measure exactly the supplied addresses without applying discovery sampling,
/// regional admission limits, or the regional top-ten download shortlist.
/// Invalid input returns before opening a connection. Cancellation preserves
/// the same partial measurements as an ordinary scan.
pub async fn run_targeted_scan(
    config: ScanConfig,
    ips: Vec<Ipv4Addr>,
    cancellation: CancellationToken,
    callback: UpdateCallback,
) -> Result<EngineOutcome, String> {
    config.validate()?;
    let ips = unique_targeted_ips(ips)?;
    run_candidates(config, ips, ScanScope::Targeted, cancellation, callback).await
}

pub(crate) async fn run_download_test(
    config: ScanConfig,
    ips: Vec<Ipv4Addr>,
    cancellation: CancellationToken,
    callback: UpdateCallback,
) -> Result<EngineOutcome, String> {
    config.validate_download_only()?;
    let ips = unique_targeted_ips(ips)?;
    run_candidates(config, ips, ScanScope::DownloadOnly, cancellation, callback).await
}

fn unique_targeted_ips(ips: Vec<Ipv4Addr>) -> Result<Vec<Ipv4Addr>, String> {
    let mut seen = HashSet::new();
    let ips: Vec<_> = ips.into_iter().filter(|ip| seen.insert(*ip)).collect();
    if ips.is_empty() {
        return Err("请选择至少一个收藏 IP 进行测试".into());
    }
    if ips.len() > 10000 {
        return Err("一次最多测试 10000 个收藏 IP".into());
    }
    Ok(ips)
}

async fn run_candidates(
    config: ScanConfig,
    ips: Vec<Ipv4Addr>,
    scope: ScanScope,
    cancellation: CancellationToken,
    callback: UpdateCallback,
) -> Result<EngineOutcome, String> {
    let target = Url::parse(&config.target_url).map_err(|e| e.to_string())?;
    let port = target
        .port_or_known_default()
        .ok_or("测速地址没有有效端口")?;
    let mut state = State {
        progress: ScanProgress {
            total: ips.len(),
            discovered_by_region: config.regions.iter().map(|r| (r.clone(), 0)).collect(),
            ..Default::default()
        },
        results: ips.iter().map(|ip| IpResult::new(*ip)).collect(),
        last_event: Instant::now(),
    };
    if scope == ScanScope::DownloadOnly {
        let schedule = DownloadQueue::from_batches(
            (0..ips.len()).map(|index| vec![index]),
            config.download_rounds,
        );
        state.stage(
            "downloading",
            schedule.remaining_rounds,
            format!("对 {} 个收藏 IP 仅测下载", ips.len()),
            &callback,
        );
        if !run_downloads(
            &mut state,
            schedule,
            &ips,
            &config,
            &target,
            scope,
            &cancellation,
            &callback,
        )
        .await
        {
            return Ok(state.cancel(&config, scope, &callback));
        }
        state.progress.stage = "completed".into();
        let completed = state
            .results
            .iter()
            .filter(|item| item.has_complete_download(&config))
            .count();
        state.progress.message = format!(
            "下载测速结束，{completed} / {} 个 IP 测完全部轮次",
            ips.len()
        );
        state.emit(&callback, true);
        return Ok(EngineOutcome {
            progress: state.progress,
            results: state.results,
            cancelled: false,
        });
    }
    if scope == ScanScope::Regional {
        state.progress.network_path = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(state.cancel(&config, scope, &callback)),
            path = crate::network::capture_async(ips[0]) => path,
        };
    }
    state.stage(
        "connecting",
        ips.len(),
        if scope == ScanScope::Targeted {
            format!("TCP 连接检查：{} 个指定 IPv4", ips.len())
        } else {
            format!("TCP 连接筛选：{} 个抽样 IPv4", ips.len())
        },
        &callback,
    );
    // Bound concurrent route queries. macOS runs one OS process per query.
    let route_queries = tokio::sync::Semaphore::new(4);
    let tcp = stream::iter(ips.iter().copied().enumerate().map(|(index, ip)| {
        let deadline = Duration::from_millis(config.connect_timeout_ms);
        let route_queries = &route_queries;
        async move {
            let path = if scope == ScanScope::Targeted {
                let _permit = route_queries
                    .acquire()
                    .await
                    .expect("route limiter stays open");
                crate::network::capture_async(ip).await
            } else {
                None
            };
            let outcome = isolated_tcp_probe(SocketAddr::new(IpAddr::V4(ip), port), deadline).await;
            (index, path, outcome)
        }
    }))
    .buffer_unordered(config.tcp_concurrency);
    tokio::pin!(tcp);
    while let Some((index, path, outcome)) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => { return Ok(state.cancel(&config, scope, &callback)); }
        next = tcp.next() => next
    } {
        if scope == ScanScope::Targeted {
            if index == 0 {
                state.progress.network_path = path.clone();
            }
            state.results[index].network_path = path;
        }
        match outcome {
            Ok(ms) => apply_tcp_measurement(&mut state.results[index], ms, &config),
            Err(failure) => {
                let result = &mut state.results[index];
                result.exclude(ExclusionCode::TcpConnect, failure.exclusion_reason());
                result.tcp_connect_failure = Some(failure);
            }
        }
        state.progress.completed += 1;
        state.emit(&callback, false);
    }

    // Discover EVERY TCP-reachable candidate before regional prioritization.
    // A global low-latency truncation would systematically exclude distant POPs.
    let reachable = pop_discovery_candidates(&state.results, &ips);
    state.stage(
        "discovering",
        reachable.len(),
        "识别当前网络实际响应的 POP，再按国家／地区分组".into(),
        &callback,
    );
    let mut timings: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
    let discoveries = stream::iter(reachable.into_iter().map(|(index, ip)| {
        let target = &target;
        let config = &config;
        async move { (index, trace_probe(config, target, ip).await) }
    }))
    .buffer_unordered(config.tcp_concurrency.min(24));
    tokio::pin!(discoveries);
    while let Some((index, outcome)) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => { return Ok(state.cancel(&config, scope, &callback)); }
        next = discoveries.next() => next
    } {
        let item = &mut state.results[index];
        item.attempts = 1;
        match outcome {
            Ok(probe) => {
                item.success_count = 1;
                item.https_ms = probe.https_ms;
                timings.insert(index, vec![probe.https_ms]);
                observe_colo(item, &probe.colo);
                *state
                    .progress
                    .discovered_by_region
                    .entry(item.region_code.clone())
                    .or_default() += 1;
                if item.region_code == "UNKNOWN" {
                    item.exclude(
                        ExclusionCode::PopUnknown,
                        format!("POP {} 尚无地区映射，保留观测但不推测归属", item.colo),
                    );
                } else if scope == ScanScope::Regional
                    && !config.regions.contains(&item.region_code)
                {
                    item.exclude(
                        ExclusionCode::RegionMismatch,
                        "实际响应地区不在本次选择范围",
                    );
                }
            }
            Err(failure) => item.exclude(failure.code, failure.message),
        }
        state.progress.completed += 1;
        state.emit(&callback, false);
    }

    // Targeted measurements have no geographical admission filter. Keep this
    // expanded configuration local: the caller's recorded settings are intact.
    let config = measurement_config(&config, &state.results, scope);
    let groups = choose_stability_candidates(&state.results, &config, scope);
    let selected: HashSet<usize> = groups.values().flatten().copied().collect();
    for (index, item) in state.results.iter_mut().enumerate() {
        if !item.is_excluded() && !selected.contains(&index) {
            item.exclude(
                ExclusionCode::RegionalAdmission,
                format!(
                    "按初筛 TCP 延迟排序，未进入本地区前 {} 名精测候选（未精测）",
                    regional_precision_limit(&config)
                ),
            );
        }
    }
    // Finish the whole bounded regional pool before choosing any downloads.
    // Only two cumulative connection failures can stop an individual probe early.
    let stability_candidates = download_schedule(&groups, &config.regions, 1);
    state.progress.precision_batch = Some(Arc::new(StabilityBatch {
        number: 1,
        candidate_ips: stability_candidates
            .iter()
            .map(|index| state.results[*index].ip.clone())
            .collect(),
    }));
    let attempts = config
        .stability_attempts
        .expect("validated stability count");
    state.begin_stability_batch(1, &stability_candidates, attempts, &callback);
    let mut progress_updates = ProgressUpdates::new(&state, &callback);
    let (updates, mut stability_updates) = tokio::sync::mpsc::unbounded_channel();
    let mut pending = stability_candidates.iter().copied();
    let mut stability_workers = tokio::task::JoinSet::new();
    for _ in 0..config.tcp_concurrency.min(4) {
        if let Some(index) = pending.next() {
            spawn_stability_worker(
                &mut stability_workers,
                index,
                SocketAddr::new(IpAddr::V4(ips[index]), port),
                &config,
                cancellation.clone(),
                updates.clone(),
            );
        }
    }
    while !stability_workers.is_empty() {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                stability_workers.abort_all();
                while let Some(result) = stability_workers.join_next().await {
                    if let Ok((index, snapshot)) = result {
                        apply_stability_snapshot(&mut state, index, snapshot);
                    }
                }
                drain_stability_updates(&mut state, &mut stability_updates);
                return Ok(state.cancel(&config, scope, &callback));
            }
            Some((index, snapshot)) = stability_updates.recv() => {
                if apply_stability_snapshot(&mut state, index, snapshot) {
                    progress_updates.changed.insert(index);
                    progress_updates.emit(&mut state, false);
                }
            }
            result = stability_workers.join_next() => {
                if let Some(Ok((index, snapshot))) = result {
                    if apply_stability_snapshot(&mut state, index, snapshot) {
                        progress_updates.changed.insert(index);
                        progress_updates.emit(&mut state, false);
                    }
                }
                if let Some(index) = pending.next() {
                    spawn_stability_worker(&mut stability_workers, index, SocketAddr::new(IpAddr::V4(ips[index]), port), &config, cancellation.clone(), updates.clone());
                }
            }
        }
    }
    drop(updates);
    drain_stability_updates(&mut state, &mut stability_updates);
    if cancellation.is_cancelled() {
        return Ok(state.cancel(&config, scope, &callback));
    }
    for index in &stability_candidates {
        if let Some((code, reason)) = state.results[*index].stability_exclusion_details(&config) {
            state.results[*index].exclude(code, reason);
        }
    }

    let download_groups = choose_download_candidates(&mut state.results, &groups, &config, scope);
    let download_candidates = download_schedule(&download_groups, &config.regions, 1);
    state.progress.precision_batch = Some(Arc::new(StabilityBatch {
        number: 1,
        candidate_ips: download_candidates
            .iter()
            .map(|index| state.results[*index].ip.clone())
            .collect(),
    }));
    state.stage(
        "verifying",
        download_candidates.len(),
        if scope == ScanScope::Targeted {
            format!(
                "TCP 稳定性已全部结束，验证全部通过稳定性检查的指定 IP：每个 IP 共验证 {} 次 HTTPS",
                config.verification_attempts
            )
        } else {
            format!(
                "TCP 稳定性已全部结束，各区择优最多 {DOWNLOAD_CANDIDATES_PER_REGION} 个：每个候选共验证 {} 次 HTTPS",
                config.verification_attempts
            )
        },
        &callback,
    );
    if !verify_candidates(
        &mut state,
        &download_candidates,
        &ips,
        &mut timings,
        &config,
        &cancellation,
        &callback,
        |ip| trace_probe(&config, &target, ip),
    )
    .await
    {
        return Ok(state.cancel(&config, scope, &callback));
    }

    let verified_groups = download_groups
        .into_iter()
        .map(|(region, entries)| {
            let entries = entries
                .into_iter()
                .filter(|index| !state.results[*index].is_excluded())
                .collect();
            (region, entries)
        })
        .collect();
    let schedule = DownloadQueue::new(&verified_groups, &config.regions, config.download_rounds);
    state.stage(
        "downloading",
        schedule.remaining_rounds,
        if scope == ScanScope::Targeted {
            if config.download_budget_enabled {
                "对全部验证通过的指定 IP 进行下载复测，共享本轮下载预算".into()
            } else {
                "对全部验证通过的指定 IP 进行下载复测，不限制下载流量".into()
            }
        } else {
            format!("各区仅对稳定性择优的前 {DOWNLOAD_CANDIDATES_PER_REGION} 个进行下载复测")
        },
        &callback,
    );
    if !run_downloads(
        &mut state,
        schedule,
        &ips,
        &config,
        &target,
        scope,
        &cancellation,
        &callback,
    )
    .await
    {
        return Ok(state.cancel(&config, scope, &callback));
    }

    for item in &mut state.results {
        if !item.is_excluded() && item.download_samples_mbps.len() < config.download_rounds {
            item.exclude(
                ExclusionCode::DownloadIncomplete,
                "未完成必要的下载复测轮次",
            );
        }
    }
    rank_results(&mut state.results, &config, true);
    let available = state
        .results
        .iter()
        .filter(|r| r.role.as_deref() == Some("primary"))
        .count();
    state.progress.stage = "completed".into();
    state.progress.stability_batch = None;
    state.progress.precision_batch = None;
    state.progress.message = if scope == ScanScope::Targeted {
        let passed = state
            .results
            .iter()
            .filter(|item| item.measurement_passed)
            .count();
        format!(
            "指定 IP 测试完成，{passed} / {} 个通过测量",
            state.results.len()
        )
    } else if available == 0 {
        let passed = state
            .results
            .iter()
            .filter(|result| result.measurement_passed)
            .count();
        if passed > 0 {
            format!("本轮 {passed} 个 IP 验证通过，但低于日常目标；可查看全部测速结果")
        } else {
            "测试完成，未找到达标推荐；请查看各区发现情况和候选排除原因".into()
        }
    } else {
        format!(
            "测试完成，{} / {} 个目标地区获得达标推荐",
            available,
            config.regions.len()
        )
    };
    state.emit(&callback, true);
    Ok(EngineOutcome {
        progress: state.progress,
        results: state.results,
        cancelled: false,
    })
}

async fn run_downloads(
    state: &mut State,
    mut schedule: DownloadQueue,
    ips: &[Ipv4Addr],
    config: &ScanConfig,
    target: &Url,
    scope: ScanScope,
    cancellation: &CancellationToken,
    callback: &UpdateCallback,
) -> bool {
    let total_budget = config.download_budget_bytes();
    let mut used = 0_u64;
    let mut updates = ProgressUpdates::new(state, callback);
    while let Some(slot) = schedule.next(total_budget, used) {
        if cancellation.is_cancelled() {
            state.progress.downloaded_bytes = used;
            return false;
        }
        let (index, allowance) = match slot {
            DownloadSlot::Round { index, allowance } => (index, allowance),
            DownloadSlot::Unfunded(index) => {
                state.results[index].exclude(
                    ExclusionCode::DownloadBudget,
                    "剩余下载预算不足以安排全部复测轮次",
                );
                state.progress.completed += config.download_rounds;
                updates.changed.insert(index);
                updates.emit(state, false);
                continue;
            }
        };
        let label = if state.results[index].city.is_empty() {
            state.results[index].colo.clone()
        } else {
            state.results[index].city.clone()
        };
        let ip = &state.results[index].ip;
        let address = if label.is_empty() {
            ip.clone()
        } else {
            format!("{label} · {ip}")
        };
        state.progress.message = format!(
            "下载复测 {address} · 第 {} / {} 轮",
            state.results[index].download_samples_mbps.len() + 1,
            config.download_rounds
        );
        updates.emit(state, false);

        if scope == ScanScope::DownloadOnly && state.results[index].download_samples_mbps.is_empty()
        {
            let ip = ips[index];
            let path = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return false,
                path = crate::network::capture_async(ip) => path,
            };
            if index == 0 {
                state.progress.network_path.clone_from(&path);
            }
            state.results[index].network_path = path;
        }

        // Poll the byte counter while streaming so progress and cancellation stay
        // responsive even when a server delivers a slow or endless response.
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter_for_probe = counter.clone();
        let download = download_probe(config, target, ips[index], allowance, counter_for_probe);
        tokio::pin!(download);
        let mut ticker = tokio::time::interval(EVENT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let measured = loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    state.progress.downloaded_bytes = used + counter.load(std::sync::atomic::Ordering::Relaxed);
                    return false;
                }
                result = &mut download => break result,
                _ = ticker.tick() => {
                    state.progress.downloaded_bytes = used + counter.load(std::sync::atomic::Ordering::Relaxed);
                    updates.emit(state, false);
                }
            }
        };
        used += counter.load(std::sync::atomic::Ordering::Relaxed);
        state.progress.downloaded_bytes = used;
        match measured {
            Ok(sample) if scope == ScanScope::DownloadOnly => {
                let item = &mut state.results[index];
                record_download_sample(item, sample.mbps);
                if let Some(colo) = &sample.colo {
                    record_observed_colo(item, colo);
                }
                item.download_sample_colos.push(sample.colo);
            }
            Ok(sample) => {
                if !record_download(
                    state,
                    index,
                    sample,
                    config,
                    cancellation,
                    &mut updates,
                    || trace_probe(config, target, ips[index]),
                )
                .await
                {
                    return false;
                }
            }
            Err(failure) => state.results[index].exclude(failure.code, failure.message),
        }
        if state.results[index].is_excluded() {
            state.progress.completed += schedule.skip_remaining_rounds(index);
        }
        state.progress.completed += 1;
        updates.changed.insert(index);
        updates.emit(state, false);
    }

    !cancellation.is_cancelled()
}

fn apply_stability_snapshot(
    state: &mut State,
    index: usize,
    snapshot: crate::stability::TcpStability,
) -> bool {
    if state.progress.stage != "stabilizing" {
        return false;
    }
    if state
        .progress
        .stability_batch
        .as_ref()
        .is_some_and(|batch| !batch.candidate_ips.contains(&state.results[index].ip))
    {
        return false;
    }
    let previous = state.results[index].tcp_stability.as_ref();
    let previous_attempted = previous.map_or(0, |stats| stats.attempted);
    if snapshot.attempted < previous_attempted
        || previous.is_some_and(|stats| {
            (stats.completed && !snapshot.completed)
                || (stats.early_rejection.is_some() && snapshot.early_rejection.is_none())
        })
    {
        return false;
    }
    state.progress.completed += snapshot.attempted - previous_attempted;
    let prefix = state
        .progress
        .stability_batch
        .as_ref()
        .map(|batch| format!("第 {} 批 · ", batch.number))
        .unwrap_or_default();
    state.progress.message = if let Some(reason) = &snapshot.early_rejection {
        state.results[index].exclude(ExclusionCode::StabilityEarly, reason.clone());
        format!(
            "{prefix}提前淘汰 {} · 已测 {} / {}：{reason}",
            state.results[index].ip, snapshot.attempted, snapshot.requested
        )
    } else {
        format!(
            "{prefix}TCP 稳定性 {} · 已完成 {} / {}，连接失败 {} 次，耗时高于目标样本 {} 次",
            state.results[index].ip,
            snapshot.attempted,
            snapshot.requested,
            snapshot.failed,
            snapshot.over_limit_count
        )
    };
    state.results[index].tcp_stability = Some(snapshot);
    true
}

fn drain_stability_updates(
    state: &mut State,
    updates: &mut tokio::sync::mpsc::UnboundedReceiver<(usize, crate::stability::TcpStability)>,
) {
    while let Ok((index, snapshot)) = updates.try_recv() {
        apply_stability_snapshot(state, index, snapshot);
    }
}

fn spawn_stability_worker(
    workers: &mut tokio::task::JoinSet<(usize, crate::stability::TcpStability)>,
    index: usize,
    address: SocketAddr,
    config: &ScanConfig,
    cancellation: CancellationToken,
    updates: tokio::sync::mpsc::UnboundedSender<(usize, crate::stability::TcpStability)>,
) {
    let attempts = config
        .stability_attempts
        .expect("validated stability count");
    let interval = Duration::from_millis(config.stability_interval_ms);
    let timeout = Duration::from_millis(config.connect_timeout_ms);
    let limit = config.max_tcp_ms.expect("validated TCP gate");
    workers.spawn(async move {
        let stats = crate::stability::probe_tcp_with_failure_limit(
            address,
            attempts,
            interval,
            timeout,
            limit,
            cancellation,
            move |snapshot| {
                let _ = updates.send((index, snapshot.clone()));
            },
        )
        .await;
        (index, stats)
    });
}

async fn isolated_tcp_probe(addr: SocketAddr, timeout: Duration) -> Result<f64, TcpConnectFailure> {
    // Record completion on an independent worker; a slow progress consumer must
    // not delay polling an already-ready socket and inflate its elapsed time.
    let worker =
        tokio_util::task::AbortOnDropHandle::new(tokio::spawn(measure_tcp_connect(addr, timeout)));
    worker.await.map_err(|error| TcpConnectFailure {
        kind: TcpConnectFailureKind::Worker,
        elapsed_ms: None,
        message: Some(error.to_string()),
    })?
}

fn apply_tcp_measurement(result: &mut IpResult, milliseconds: f64, config: &ScanConfig) {
    result.tcp_ms = milliseconds;
    if let Some(limit) = config.max_tcp_ms.filter(|_| config.tcp_latency_screening) {
        if milliseconds > limit {
            result.measurement_passed = false;
            result.eligible = false;
            result.role = Some("candidate".into());
            result.exclude(
                ExclusionCode::TcpLatency,
                format!("TCP 延迟 {milliseconds} ms，超过上限 {limit} ms"),
            );
        }
    }
}

fn pop_discovery_candidates(results: &[IpResult], ips: &[Ipv4Addr]) -> Vec<(usize, Ipv4Addr)> {
    results
        .iter()
        .enumerate()
        .filter(|(_, result)| !result.is_excluded())
        .map(|(index, _)| (index, ips[index]))
        .collect()
}

fn client_for_ip(config: &ScanConfig, target: &Url, ip: Ipv4Addr) -> Result<Client, String> {
    client_builder_for_ip(config, target, ip)?
        .build()
        .map_err(|e| format!("无法创建 HTTPS 测速连接：{e}"))
}

fn client_builder_for_ip(
    config: &ScanConfig,
    target: &Url,
    ip: Ipv4Addr,
) -> Result<reqwest::ClientBuilder, String> {
    let host = target.host_str().ok_or("测速地址缺少域名")?;
    let addr = SocketAddr::new(
        IpAddr::V4(ip),
        target.port_or_known_default().ok_or("测速地址缺少端口")?,
    );
    Ok(Client::builder()
        .no_proxy()
        .https_only(true)
        .http1_only()
        .redirect(Policy::none())
        .pool_max_idle_per_host(0)
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
        .resolve(host, addr)
        .user_agent(concat!("IP-Picker/", env!("CARGO_PKG_VERSION"))))
}

#[derive(Debug)]
struct ProbeError {
    code: ExclusionCode,
    message: String,
}

impl ProbeError {
    fn new(code: ExclusionCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn validate_response(response: &Response, ip: Ipv4Addr) -> Result<(), ProbeError> {
    match response.remote_addr() {
        Some(peer) if peer.ip() == IpAddr::V4(ip) => {}
        Some(peer) => {
            return Err(ProbeError::new(
                ExclusionCode::PopDrift,
                format!("连接对端发生变化（{peer}），已拒绝使用该测量"),
            ));
        }
        None => {
            return Err(ProbeError::new(
                ExclusionCode::PopDrift,
                "无法确认实际连接的候选 IP",
            ));
        }
    }
    let status = response.status();
    if status.is_redirection() {
        return Err(ProbeError::new(
            ExclusionCode::HttpsFailure,
            format!(
                "HTTP {} 重定向已拒绝，防止测速跳转至其他 IP",
                status.as_u16()
            ),
        ));
    }
    if !status.is_success() {
        return Err(ProbeError::new(
            ExclusionCode::HttpsFailure,
            format!("HTTPS 响应状态不符合预期：HTTP {}", status.as_u16()),
        ));
    }
    Ok(())
}

struct TraceProbe {
    https_ms: f64,
    colo: String,
}

/// Keep one request per active candidate in flight. Every completed request is
/// applied before starting its successor, so cancellation cannot discard an
/// earlier success that was held in a candidate-local clone.
async fn verify_candidates<F, Fut>(
    state: &mut State,
    candidates: &[usize],
    ips: &[Ipv4Addr],
    timings: &mut BTreeMap<usize, Vec<f64>>,
    config: &ScanConfig,
    cancellation: &CancellationToken,
    callback: &UpdateCallback,
    probe: F,
) -> bool
where
    F: Fn(Ipv4Addr) -> Fut,
    Fut: std::future::Future<Output = Result<TraceProbe, ProbeError>>,
{
    let pending: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|index| {
            let item = &state.results[*index];
            item.attempts < config.verification_attempts && !item.is_excluded()
        })
        .collect();
    state.progress.completed += candidates.len() - pending.len();
    let mut pending = pending.into_iter();
    let mut requests = stream::FuturesUnordered::new();
    for _ in 0..config.tcp_concurrency.min(16) {
        if let Some(index) = pending.next() {
            requests.push(verification_request(index, ips[index], &probe));
        }
    }
    while let Some((index, outcome)) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => { return false; }
        next = requests.next() => next
    } {
        let item = &mut state.results[index];
        item.attempts += 1;
        let samples = timings.entry(index).or_default();
        match outcome {
            Ok(probe) => {
                item.success_count += 1;
                samples.push(probe.https_ms);
                observe_colo(item, &probe.colo);
            }
            Err(failure) => {
                item.exclude(failure.code, format!("HTTPS 复测失败：{}", failure.message));
            }
        }
        item.https_ms = median(samples);
        item.jitter_ms = spread(samples);
        // One failed service check or cross-region POP observation excludes this
        // candidate immediately; other candidates can use its released slot.
        if item.attempts < config.verification_attempts && !item.is_excluded() {
            requests.push(verification_request(index, ips[index], &probe));
        } else {
            state.progress.completed += 1;
            if let Some(index) = pending.next() {
                requests.push(verification_request(index, ips[index], &probe));
            }
        }
        state.emit(callback, false);
    }
    true
}

async fn verification_request<F, Fut>(
    index: usize,
    ip: Ipv4Addr,
    probe: &F,
) -> (usize, Result<TraceProbe, ProbeError>)
where
    F: Fn(Ipv4Addr) -> Fut,
    Fut: std::future::Future<Output = Result<TraceProbe, ProbeError>>,
{
    (index, probe(ip).await)
}

fn record_download_sample(item: &mut IpResult, mbps: f64) {
    item.download_samples_mbps.push(mbps);
    item.download_mbps = median(&item.download_samples_mbps);
}

async fn record_download<F, Fut>(
    state: &mut State,
    index: usize,
    sample: DownloadProbe,
    config: &ScanConfig,
    cancellation: &CancellationToken,
    updates: &mut ProgressUpdates<'_>,
    probe: F,
) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<TraceProbe, ProbeError>>,
{
    // The download has finished even if its additional POP check is cancelled.
    let item = &mut state.results[index];
    record_download_sample(item, sample.mbps);
    item.measurement_passed = false;
    if let Some(colo) = sample.colo {
        observe_colo(item, &colo);
    } else {
        // This explicit incomplete reason also gates a saved final-round sample
        // when cancellation happens before the same-host POP check completes.
        item.exclude(
            ExclusionCode::PopRecheckIncomplete,
            "下载后的 POP 复核未完成",
        );
        // Publish the observed bytes and sample before waiting on another
        // request, so checkpoints can preserve it if that request is interrupted.
        updates.changed.insert(index);
        updates.emit(state, true);
        let check = tokio::select! {
            biased;
            _ = cancellation.cancelled() => { return false; }
            outcome = probe() => outcome
        };
        let item = &mut state.results[index];
        item.attempts += 1;
        match check {
            Ok(probe) => {
                item.success_count += 1;
                item.clear_exclusion();
                observe_colo(item, &probe.colo);
            }
            Err(failure) => {
                item.exclude(
                    ExclusionCode::PopRecheckFailure,
                    format!("下载后的 POP 复核失败：{}", failure.message),
                );
            }
        }
    }
    let item = &mut state.results[index];
    item.measurement_passed = item.has_complete_measurement(config);
    true
}

async fn trace_probe(
    config: &ScanConfig,
    target: &Url,
    ip: Ipv4Addr,
) -> Result<TraceProbe, ProbeError> {
    let mut trace_url = target.clone();
    trace_url.set_path("/cdn-cgi/trace");
    trace_url.set_query(None);
    let client = client_for_ip(config, target, ip)
        .map_err(|message| ProbeError::new(ExclusionCode::HttpsFailure, message))?;
    let started = Instant::now();
    let mut response = client
        .get(trace_url)
        .timeout(Duration::from_millis(config.request_timeout_ms))
        .header(header::CONNECTION, "close")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(request_error)?;
    let https_ms = started.elapsed().as_secs_f64() * 1000.0;
    validate_response(&response, ip)?;
    if response.content_length().unwrap_or(0) > MAX_TRACE_BYTES as u64 {
        return Err(ProbeError::new(
            ExclusionCode::HttpsFailure,
            "trace 响应过大，目标可能不是 Cloudflare trace 端点",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if bytes.len() + chunk.len() > MAX_TRACE_BYTES {
            return Err(ProbeError::new(
                ExclusionCode::HttpsFailure,
                "trace 响应超过 16 KiB 限制",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = std::str::from_utf8(&bytes)
        .map_err(|_| ProbeError::new(ExclusionCode::HttpsFailure, "trace 响应不是有效文本"))?;
    let colo = parse_trace(body, target)?;
    Ok(TraceProbe { https_ms, colo })
}

fn parse_trace(body: &str, target: &Url) -> Result<String, ProbeError> {
    let mut colo = None;
    let mut host = None;
    for line in body.lines() {
        if let Some(value) = line.strip_prefix("colo=") {
            if colo.is_some() {
                return Err(ProbeError::new(
                    ExclusionCode::PopUnknown,
                    "trace 含有重复 colo 字段",
                ));
            }
            colo = Some(value.trim());
        }
        if let Some(value) = line.strip_prefix("h=") {
            if host.is_some() {
                return Err(ProbeError::new(
                    ExclusionCode::HttpsFailure,
                    "trace 含有重复 h 字段",
                ));
            }
            host = Some(value.trim());
        }
    }
    let colo = colo
        .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase()))
        .ok_or_else(|| {
            ProbeError::new(
                ExclusionCode::PopUnknown,
                "未获得有效的 Cloudflare POP 标识（colo）",
            )
        })?;
    let expected = target
        .host_str()
        .ok_or_else(|| ProbeError::new(ExclusionCode::HttpsFailure, "测速地址缺少域名"))?;
    let expected_port = target.port().map(|p| format!("{expected}:{p}"));
    if !host.is_some_and(|h| {
        h.eq_ignore_ascii_case(expected)
            || expected_port
                .as_deref()
                .is_some_and(|p| h.eq_ignore_ascii_case(p))
    }) {
        return Err(ProbeError::new(
            ExclusionCode::HttpsFailure,
            "trace 未确认测速目标域名，响应内容不符合预期",
        ));
    }
    Ok(colo.to_string())
}

fn request_error(error: reqwest::Error) -> ProbeError {
    if error.is_timeout() {
        return ProbeError::new(ExclusionCode::HttpsTimeout, "HTTPS 请求超时");
    }
    let mut details = error.to_string();
    let mut cause = std::error::Error::source(&error);
    for _ in 0..4 {
        if let Some(source) = cause {
            details.push_str(": ");
            details.push_str(&source.to_string());
            cause = source.source();
        } else {
            break;
        }
    }
    let message = if details.to_ascii_lowercase().contains("certificate") {
        format!("TLS 证书验证失败：{details}")
    } else {
        format!("HTTPS 请求失败：{details}")
    };
    ProbeError::new(ExclusionCode::HttpsFailure, message)
}

struct DownloadProbe {
    mbps: f64,
    colo: Option<String>,
}

/// When enabled, the budget counts application-read response bytes, not TLS/HTTP
/// overhead or packets already in flight. Only budgeted requests require the
/// server to provide a known body length within our Range. All requests retain
/// the configured per-round deadline and the same minimum sample size.
async fn download_probe(
    config: &ScanConfig,
    target: &Url,
    ip: Ipv4Addr,
    allowance: Option<u64>,
    counter: Arc<std::sync::atomic::AtomicU64>,
) -> Result<DownloadProbe, ProbeError> {
    let client = client_for_ip(config, target, ip)
        .map_err(|message| ProbeError::new(ExclusionCode::HttpsFailure, message))?;
    let (url, requested_bytes) = match allowance {
        Some(bytes) => {
            let (url, bytes) = bounded_download_url(target, bytes);
            (url, Some(bytes))
        }
        None => (target.clone(), None),
    };
    let mut request = client
        .get(url)
        .header(header::CONNECTION, "close")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ACCEPT_ENCODING, "identity");
    if let Some(bytes) = requested_bytes {
        request = request.header(header::RANGE, format!("bytes=0-{}", bytes - 1));
    }
    measure_download_request(config, request, ip, requested_bytes, counter).await
}

async fn measure_download_request(
    config: &ScanConfig,
    request: reqwest::RequestBuilder,
    ip: Ipv4Addr,
    requested_bytes: Option<u64>,
    counter: Arc<std::sync::atomic::AtomicU64>,
) -> Result<DownloadProbe, ProbeError> {
    let started = Instant::now();
    let response = tokio::time::timeout(
        Duration::from_millis(config.request_timeout_ms),
        request.send(),
    )
    .await
    .map_err(|_| ProbeError::new(ExclusionCode::HttpsTimeout, "等待下载响应头超时"))?
    .map_err(request_error)?;
    measure_download_response(config, response, ip, requested_bytes, started, counter).await
}

async fn measure_download_response(
    config: &ScanConfig,
    mut response: Response,
    ip: Ipv4Addr,
    requested_bytes: Option<u64>,
    started: Instant,
    counter: Arc<std::sync::atomic::AtomicU64>,
) -> Result<DownloadProbe, ProbeError> {
    validate_response(&response, ip)?;
    if !has_unencoded_body(response.headers()) {
        return Err(ProbeError::new(
            ExclusionCode::DownloadFailure,
            "下载响应进行了压缩，无法可靠比较原始下载吞吐",
        ));
    }
    if requested_bytes
        .is_some_and(|limit| response.content_length().is_none_or(|bytes| bytes > limit))
    {
        return Err(ProbeError::new(
            ExclusionCode::DownloadFailure,
            "下载目标未返回预算内的明确内容长度；请使用支持 Range 的静态文件或默认测速地址",
        ));
    }
    let expected_bytes = response.content_length();
    if expected_bytes.is_some_and(|bytes| bytes < MIN_SAMPLE_BYTES) {
        return Err(ProbeError::new(
            ExclusionCode::DownloadFailure,
            "下载响应不足 64 KiB，样本过小；请使用较大的测试文件或增加数据预算",
        ));
    }
    let colo = ray_colo(response.headers());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.download_seconds);
    let mut received = 0_u64;
    loop {
        let next = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => break,
            next = response.chunk() => next.map_err(request_error)?
        };
        match next {
            Some(bytes) => {
                // Count actual received data in both budget modes. Budgeted
                // requests have already checked the HTTP framing above.
                received += bytes.len() as u64;
                counter.store(received, std::sync::atomic::Ordering::Relaxed);
                if expected_bytes.is_some_and(|bytes| received >= bytes) {
                    break;
                }
            }
            None => break,
        }
    }
    if received < MIN_SAMPLE_BYTES {
        return Err(ProbeError::new(
            ExclusionCode::DownloadFailure,
            "下载样本不足 64 KiB，时限内未获得有效测速数据",
        ));
    }
    // Include connection/TLS/header setup: this is effective single-request
    // throughput, consistently measured across all candidates.
    let mbps = received as f64 * 8.0 / started.elapsed().as_secs_f64().max(0.000001) / 1_000_000.0;
    Ok(DownloadProbe { mbps, colo })
}

fn has_unencoded_body(headers: &header::HeaderMap) -> bool {
    let content_is_identity = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .all(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .all(|coding| coding.trim().eq_ignore_ascii_case("identity"))
            })
        });
    // The HTTP client removes chunk framing, while other transfer codings may
    // still be present in the body returned to the measurement code.
    let mut transfer_codings = headers.get_all(header::TRANSFER_ENCODING).iter();
    let transfer_is_unencoded = transfer_codings.next().is_none_or(|value| {
        value
            .to_str()
            .is_ok_and(|value| value.trim().eq_ignore_ascii_case("chunked"))
            && transfer_codings.next().is_none()
    });
    content_is_identity && transfer_is_unencoded
}

fn bounded_download_url(target: &Url, allowance: u64) -> (Url, u64) {
    let mut url = target.clone();
    let mut requested_bytes = allowance;
    // This is the documented Cloudflare speed-test endpoint. Only rewrite its
    // known parameter; arbitrary custom URL queries must retain their meaning.
    if target.host_str() == Some("speed.cloudflare.com") && target.path() == "/__down" {
        let pairs: Vec<(String, String)> = target
            .query_pairs()
            .map(|(a, b)| (a.into_owned(), b.into_owned()))
            .collect();
        if let Some(original) = pairs
            .iter()
            .find(|(key, _)| key == "bytes")
            .and_then(|(_, val)| val.parse::<u64>().ok())
        {
            requested_bytes = requested_bytes.min(original.max(1));
        }
        let mut query = url.query_pairs_mut();
        query.clear();
        for (key, value) in pairs.into_iter().filter(|(k, _)| k != "bytes") {
            query.append_pair(&key, &value);
        }
        query.append_pair("bytes", &requested_bytes.to_string());
    }
    (url, requested_bytes.max(1))
}

fn ray_colo(headers: &header::HeaderMap) -> Option<String> {
    let mut values = headers.get_all("cf-ray").iter();
    let (id, suffix) = values.next()?.to_str().ok()?.split_once('-')?;
    (values.next().is_none()
        && !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_hexdigit())
        && suffix.len() == 3
        && suffix.bytes().all(|b| b.is_ascii_uppercase()))
    .then(|| suffix.to_string())
}

fn record_observed_colo(item: &mut IpResult, colo: &str) {
    if !item.observed_colos.iter().any(|c| c == colo) {
        item.observed_colos.push(colo.into());
    }
    if item.colo.is_empty() {
        item.colo = colo.into();
        if let Some((region, city)) = pop::location(colo) {
            item.region_code = region.into();
            item.city = city.into();
        }
    }
}

fn observe_colo(item: &mut IpResult, colo: &str) {
    record_observed_colo(item, colo);
    if colo != item.colo {
        match pop::location(colo) {
            Some((region, _)) if region != item.region_code => {
                item.exclude(
                    ExclusionCode::PopDrift,
                    format!(
                        "接入地区发生变化：{} → {colo}，暂不作为固定地区推荐",
                        item.colo
                    ),
                );
            }
            None => {
                item.exclude(
                    ExclusionCode::PopDrift,
                    format!("POP 变化至未知地区 {colo}，无法确认地区归属"),
                );
            }
            _ => {}
        }
    }
}

fn measurement_config(config: &ScanConfig, results: &[IpResult], scope: ScanScope) -> ScanConfig {
    let mut effective = config.clone();
    if scope == ScanScope::Targeted {
        for result in results {
            if pop::location(&result.colo).is_some_and(|(region, _)| region == result.region_code)
                && !effective.regions.contains(&result.region_code)
            {
                effective.regions.push(result.region_code.clone());
            }
        }
    }
    effective
}

fn choose_stability_candidates(
    results: &[IpResult],
    config: &ScanConfig,
    scope: ScanScope,
) -> BTreeMap<String, Vec<usize>> {
    if scope == ScanScope::Regional {
        return choose_per_region(results, config);
    }
    let mut groups = BTreeMap::<String, Vec<usize>>::new();
    for (index, result) in results.iter().enumerate() {
        if !result.is_excluded() && result.region_code != "UNKNOWN" {
            groups
                .entry(result.region_code.clone())
                .or_default()
                .push(index);
        }
    }
    groups
}

fn choose_per_region(results: &[IpResult], config: &ScanConfig) -> BTreeMap<String, Vec<usize>> {
    config
        .regions
        .iter()
        .map(|region| {
            let mut entries: Vec<_> = results
                .iter()
                .enumerate()
                .filter(|(_, r)| r.region_code == *region && !r.is_excluded())
                .map(|(i, _)| i)
                .collect();
            entries.sort_by(|a, b| {
                results[*a]
                    .tcp_ms
                    .total_cmp(&results[*b].tcp_ms)
                    .then(results[*a].https_ms.total_cmp(&results[*b].https_ms))
                    .then(results[*a].ip.cmp(&results[*b].ip))
            });
            // Latency order must remain strict: a slower distinct /24 must not
            // displace a faster address from the same prefix in the top queue.
            entries.truncate(regional_precision_limit(config));
            (region.clone(), entries)
        })
        .collect()
}

fn regional_precision_limit(config: &ScanConfig) -> usize {
    let regional_limit = config.regional_candidate_limit.unwrap_or(usize::MAX);
    if config.precision_mode == "limited" {
        regional_limit.min(config.candidates_per_region)
    } else {
        regional_limit
    }
}

/// Download admission is based on complete stability evidence from the whole
/// retained pool. The download preference cannot rank unknown bandwidth yet.
fn choose_download_per_region(
    results: &mut [IpResult],
    groups: &BTreeMap<String, Vec<usize>>,
    config: &ScanConfig,
) -> BTreeMap<String, Vec<usize>> {
    choose_download_groups(
        results,
        groups,
        config,
        Some(DOWNLOAD_CANDIDATES_PER_REGION),
    )
}

fn choose_download_candidates(
    results: &mut [IpResult],
    groups: &BTreeMap<String, Vec<usize>>,
    config: &ScanConfig,
    scope: ScanScope,
) -> BTreeMap<String, Vec<usize>> {
    if scope == ScanScope::Regional {
        choose_download_per_region(results, groups, config)
    } else {
        choose_download_groups(results, groups, config, None)
    }
}

fn choose_download_groups(
    results: &mut [IpResult],
    groups: &BTreeMap<String, Vec<usize>>,
    config: &ScanConfig,
    limit: Option<usize>,
) -> BTreeMap<String, Vec<usize>> {
    groups
        .iter()
        .map(|(region, candidates)| {
            let mut entries: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|index| {
                    !results[*index].is_excluded()
                        && results[*index].stability_exclusion(config).is_none()
                })
                .collect();
            sort_result_indices(
                &mut entries,
                results,
                "daily",
                &config.stability_latency_metric,
            );
            if let Some(limit) = limit {
                for index in entries.iter().skip(limit) {
                    results[*index].exclude(
                        ExclusionCode::DownloadShortlist,
                        format!("TCP 稳定性已完成，未进入本地区前 {limit} 名下载候选（未测速）"),
                    );
                }
                entries.truncate(limit);
            }
            (region.clone(), entries)
        })
        .collect()
}

fn download_allowance(total_budget: u64, used: u64, remaining_rounds: u64) -> u64 {
    let remaining = total_budget.saturating_sub(used);
    let fair_share = remaining / remaining_rounds.max(1);
    fair_share.max(MIN_SAMPLE_BYTES).min(remaining)
}

enum DownloadSlot {
    Round {
        index: usize,
        allowance: Option<u64>,
    },
    Unfunded(usize),
}

struct DownloadQueue {
    batches: VecDeque<VecDeque<usize>>,
    active_rounds: VecDeque<usize>,
    rounds_per_ip: usize,
    remaining_rounds: usize,
}

impl DownloadQueue {
    fn new(groups: &BTreeMap<String, Vec<usize>>, regions: &[String], rounds: usize) -> Self {
        Self::from_batches(download_batches(groups, regions), rounds)
    }

    fn from_batches(batches: impl Iterator<Item = Vec<usize>>, rounds: usize) -> Self {
        let batches: VecDeque<VecDeque<_>> = batches.map(VecDeque::from).collect();
        let remaining_rounds = batches.iter().map(VecDeque::len).sum::<usize>() * rounds;
        Self {
            batches,
            active_rounds: VecDeque::new(),
            rounds_per_ip: rounds,
            remaining_rounds,
        }
    }

    fn next(&mut self, total_budget: Option<u64>, used: u64) -> Option<DownloadSlot> {
        if self.active_rounds.is_empty() {
            while self.batches.front().is_some_and(VecDeque::is_empty) {
                self.batches.pop_front();
            }
            let batch = self.batches.front_mut()?;
            // Admit only the IPs whose minimum sample size can be funded for every
            // round. Revisit the rest of this rank if a failure frees enough budget.
            let candidates = total_budget.map_or(batch.len(), |total| {
                let per_ip = MIN_SAMPLE_BYTES * self.rounds_per_ip as u64;
                batch
                    .len()
                    .min((total.saturating_sub(used) / per_ip) as usize)
            });
            if candidates == 0 {
                self.remaining_rounds -= self.rounds_per_ip;
                return batch.pop_front().map(DownloadSlot::Unfunded);
            }
            let admitted: Vec<_> = batch.drain(..candidates).collect();
            for _ in 0..self.rounds_per_ip {
                self.active_rounds.extend(admitted.iter().copied());
            }
        }
        let index = self.active_rounds.pop_front()?;
        let allowance =
            total_budget.map(|total| download_allowance(total, used, self.remaining_rounds as u64));
        self.remaining_rounds -= 1;
        Some(DownloadSlot::Round { index, allowance })
    }

    fn skip_remaining_rounds(&mut self, index: usize) -> usize {
        let before = self.active_rounds.len();
        self.active_rounds.retain(|&candidate| candidate != index);
        let skipped = before - self.active_rounds.len();
        self.remaining_rounds -= skipped;
        skipped
    }
}

fn download_batches<'a>(
    groups: &'a BTreeMap<String, Vec<usize>>,
    region_order: &'a [String],
) -> impl Iterator<Item = Vec<usize>> + 'a {
    let max = region_order
        .iter()
        .filter_map(|region| groups.get(region))
        .map(Vec::len)
        .max()
        .unwrap_or(0);
    (0..max).map(|rank| {
        region_order
            .iter()
            .filter_map(|region| {
                groups
                    .get(region)
                    .and_then(|entries| entries.get(rank))
                    .copied()
            })
            .collect()
    })
}

fn download_schedule(
    groups: &BTreeMap<String, Vec<usize>>,
    region_order: &[String],
    rounds: usize,
) -> Vec<usize> {
    let mut plan = Vec::new();
    // Complete the first candidate of every region before spending budget on
    // later backups. Every round rotates across regions to prevent starvation.
    for batch in download_batches(groups, region_order) {
        for _ in 0..rounds {
            plan.extend_from_slice(&batch);
        }
    }
    plan
}

fn rank_results(results: &mut [IpResult], config: &ScanConfig, completed: bool) {
    for result in results.iter_mut() {
        result.role = Some("candidate".into());
        result.measurement_passed = result.has_complete_measurement(config);
        result.apply_recommendation_threshold(config);
    }
    if !completed {
        return;
    }
    for region in &config.regions {
        let mut entries: Vec<_> = results
            .iter()
            .enumerate()
            .filter(|(_, r)| r.eligible && r.region_code == *region)
            .map(|(i, _)| i)
            .collect();
        sort_result_indices(
            &mut entries,
            results,
            &config.preference,
            &config.stability_latency_metric,
        );
        for (rank, index) in entries.into_iter().take(config.ips_per_region).enumerate() {
            results[index].role = Some(if rank == 0 { "primary" } else { "backup" }.into());
        }
    }
}

fn median(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    if sorted.len().is_multiple_of(2) {
        (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
    } else {
        sorted[sorted.len() / 2]
    }
}

fn spread(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        - samples.iter().copied().fold(f64::INFINITY, f64::min)
}

/// Builtin sources use weighted /24 coverage. Custom networks retain equal
/// strata; at most the configured number is materialized, even for /0.
/// Explicit custom IPs are retained first and every output is unique.
pub fn sample_candidates(config: &ScanConfig) -> Result<Vec<Ipv4Addr>, String> {
    if config.source == "cloudflare" {
        return sample_cloudflare(config.max_candidates);
    }
    let candidates = parse_custom_candidates(&config.custom_ips).map_err(|issue| issue.message)?;
    sample_custom_candidates(candidates, config.max_candidates)
}

fn sample_custom_candidates(
    CustomCandidates { explicit, networks }: CustomCandidates,
    limit: usize,
) -> Result<Vec<Ipv4Addr>, String> {
    if !(1..=10000).contains(&limit) {
        return Err("抽样数量须为 1 至 10000".into());
    }
    let mut seen = HashSet::new();
    let mut output = Vec::with_capacity(limit);
    for ip in explicit {
        if seen.insert(ip) {
            output.push(ip);
        }
        if output.len() >= limit {
            return Ok(output);
        }
    }
    let mut explicit: Vec<_> = output.iter().map(|ip| u64::from(u32::from(*ip))).collect();
    explicit.sort_unstable();
    let pools: Vec<_> = disjoint_custom_ranges(networks)
        .into_iter()
        .filter_map(|range| {
            let first = explicit.partition_point(|&ip| ip < range.start);
            let last = explicit.partition_point(|&ip| ip < range.end);
            let pool = CustomNetwork {
                range,
                explicit: &explicit[first..last],
            };
            (pool.capacity() > 0).then_some(pool)
        })
        .collect();
    let remaining = limit - output.len();
    let mut quotas = vec![0; pools.len()];
    let mut available: VecDeque<_> = (0..pools.len()).collect();
    let mut schedule = Vec::with_capacity(remaining);
    for _ in 0..remaining {
        let Some(index) = available.pop_front() else {
            break;
        };
        quotas[index] += 1;
        schedule.push(index);
        if (quotas[index] as u64) < pools[index].capacity() {
            available.push_back(index);
        }
    }

    let mut rng = rand::thread_rng();
    let mut samples: Vec<_> = pools
        .into_iter()
        .zip(quotas)
        .map(|(pool, quota)| pool.sample(quota, &mut rng).into_iter())
        .collect();
    output.extend(schedule.into_iter().map(|index| {
        samples[index]
            .next()
            .expect("each scheduled slot has a stratified sample")
    }));
    if output.is_empty() {
        return Err("候选列表为空，请输入 IPv4 或 CIDR".into());
    }
    Ok(output)
}

fn disjoint_custom_ranges(networks: Vec<Ipv4Net>) -> Vec<Range<u64>> {
    let mut ranges: Vec<_> = networks
        .into_iter()
        .enumerate()
        .map(|(index, network)| {
            let start = u64::from(u32::from(network.network()));
            let end = start + (1_u64 << (32 - network.prefix_len()));
            (index, start..end)
        })
        .collect();
    // Aligned CIDRs can only overlap by containment. Sort wider ranges first
    // when they share a start, then restore the order of the surviving inputs.
    ranges.sort_unstable_by_key(|(_, range)| (range.start, Reverse(range.end)));
    let mut covered_end = 0;
    ranges.retain(|(_, range)| {
        if range.start < covered_end {
            return false;
        }
        covered_end = range.end;
        true
    });
    ranges.sort_unstable_by_key(|(index, _)| *index);
    ranges.into_iter().map(|(_, range)| range).collect()
}

struct CustomNetwork<'a> {
    range: Range<u64>,
    explicit: &'a [u64],
}

impl CustomNetwork<'_> {
    fn capacity(&self) -> u64 {
        self.range.end - self.range.start - self.explicit.len() as u64
    }

    fn sample(&self, quota: usize, rng: &mut impl Rng) -> Vec<Ipv4Addr> {
        let capacity = self.capacity();
        let quota = quota as u64;
        let mut explicit = self.explicit.iter().copied().peekable();
        let mut skipped = 0;
        (0..quota)
            .map(|stratum| {
                let start = stratum * capacity / quota;
                let end = (stratum + 1) * capacity / quota;
                let mut ip = self.range.start + rng.gen_range(start..end) + skipped;
                // Strata produce increasing ranks in the available addresses.
                // Advance past explicit IPs once as those ranks are mapped back.
                while explicit.peek().is_some_and(|&reserved| reserved <= ip) {
                    explicit.next();
                    skipped += 1;
                    ip += 1;
                }
                Ipv4Addr::from(ip as u32)
            })
            .collect()
    }
}

/// Cover distinct /24 routing prefixes before taking another host from any
/// prefix. Large CF ranges receive proportionally more of the fixed budget.
fn sample_cloudflare(limit: usize) -> Result<Vec<Ipv4Addr>, String> {
    if !(1..=10000).contains(&limit) {
        return Err("抽样数量须为 1 至 10000".into());
    }
    let networks = CLOUDFLARE_IPV4
        .iter()
        .map(|cidr| cidr.parse::<Ipv4Net>().map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let weights: Vec<usize> = networks
        .iter()
        .map(|net| 1_usize << (24 - net.prefix_len()))
        .collect();
    let prefix_count: usize = weights.iter().sum();
    let mut output = Vec::with_capacity(limit);
    let mut seen = HashSet::with_capacity(limit);
    let mut rng = rand::thread_rng();
    while output.len() < limit {
        let counts = weighted_network_quotas(&weights, (limit - output.len()).min(prefix_count));
        let mut round: Vec<Vec<Ipv4Addr>> = Vec::with_capacity(networks.len());
        for ((network, prefixes), count) in networks.iter().zip(&weights).zip(&counts) {
            let base = u32::from(network.network());
            let mut addresses = Vec::with_capacity(*count);
            for stratum in 0..*count {
                let start = stratum * prefixes / count;
                let end = (stratum + 1) * prefixes / count;
                let prefix = base + (rng.gen_range(start..end) as u32) * 256;
                let initial = rng.gen_range(1_u32..=254);
                // At most two sampling rounds are needed for the 10000 cap;
                // bound collision resolution even if this cap later changes.
                for offset in 0..254 {
                    let host = 1 + (initial - 1 + offset) % 254;
                    let ip = Ipv4Addr::from(prefix + host);
                    if seen.insert(ip) {
                        addresses.push(ip);
                        break;
                    }
                }
            }
            round.push(addresses);
        }
        let max = round.iter().map(Vec::len).max().unwrap_or(0);
        let before = output.len();
        for index in 0..max {
            for addresses in &round {
                if let Some(ip) = addresses.get(index) {
                    output.push(*ip);
                }
            }
        }
        if output.len() == before {
            return Err("内置网段无法生成更多唯一候选".into());
        }
    }
    Ok(output)
}

/// Integer largest-remainder allocation. When possible reserve one slot for
/// each original network, then apportion the rest by remaining /24 capacity.
fn weighted_network_quotas(weights: &[usize], total: usize) -> Vec<usize> {
    let mut counts = vec![0; weights.len()];
    if total == 0 {
        return counts;
    }
    let base = usize::from(total >= weights.len());
    counts.fill(base);
    let remaining = total - base * weights.len();
    let capacity: usize = weights.iter().map(|weight| weight - base).sum();
    if capacity == 0 {
        return counts;
    }
    let mut remainders = Vec::with_capacity(weights.len());
    for (index, weight) in weights.iter().enumerate() {
        let product = remaining * (weight - base);
        counts[index] += product / capacity;
        remainders.push((product % capacity, index));
    }
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let extra = total - counts.iter().sum::<usize>();
    for (_, index) in remainders.into_iter().take(extra) {
        counts[index] += 1;
    }
    counts
}

#[cfg(test)]
async fn read_http_fixture_headers(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::io::Result<Vec<u8>> {
    use std::io::{Error, ErrorKind};
    use tokio::io::AsyncReadExt;

    let mut request = Vec::new();
    let mut buffer = [0; 2048];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let length = stream.read(&mut buffer).await?;
        if length == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "client closed before sending HTTP headers",
            ));
        }
        request.extend_from_slice(&buffer[..length]);
        if request.len() > 16 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "oversized request headers",
            ));
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(input: &str, limit: usize) -> ScanConfig {
        ScanConfig {
            source: "custom".into(),
            custom_ips: input.into(),
            max_candidates: limit,
            ..Default::default()
        }
    }

    fn measured(ip: &str, region: &str, ms: f64, mbps: f64) -> IpResult {
        let mut result = IpResult::new(ip.parse().unwrap());
        result.region_code = region.into();
        result.colo = if region == "HK" { "HKG" } else { "NRT" }.into();
        result.observed_colos.push(result.colo.clone());
        result.tcp_ms = ms / 2.0;
        result.https_ms = ms;
        result.jitter_ms = 2.0;
        result.download_mbps = mbps;
        result.download_samples_mbps = vec![mbps, mbps];
        result.success_count = 5;
        result.attempts = 5;
        result.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(result.tcp_ms); 100],
        ));
        result
    }

    fn verification_state(rows: Vec<IpResult>) -> State {
        State {
            progress: ScanProgress::default(),
            results: rows,
            last_event: Instant::now(),
        }
    }

    async fn verify_row<F, Fut>(
        row: IpResult,
        samples: Vec<f64>,
        required_attempts: usize,
        probe: F,
    ) -> IpResult
    where
        F: Fn(Ipv4Addr) -> Fut,
        Fut: std::future::Future<Output = Result<TraceProbe, ProbeError>>,
    {
        let ips = vec![row.ip.parse().unwrap()];
        let mut state = verification_state(vec![row]);
        let config = ScanConfig {
            verification_attempts: required_attempts,
            ..Default::default()
        };
        assert!(
            verify_candidates(
                &mut state,
                &[0],
                &ips,
                &mut BTreeMap::from([(0, samples)]),
                &config,
                &CancellationToken::new(),
                &(Arc::new(|_| {}) as UpdateCallback),
                probe,
            )
            .await
        );
        assert_eq!(state.progress.completed, 1);
        state.results.remove(0)
    }

    #[test]
    fn preparing_a_scan_rejects_invalid_configuration_and_custom_sources() {
        let invalid_config = ScanConfig {
            max_candidates: 0,
            ..Default::default()
        };
        assert!(prepare_scan(invalid_config).is_err());
        for input in ["192.0.2.1\nnot-an-ip", "# comments only\n  # no addresses"] {
            assert!(
                prepare_scan(custom(input, 1)).is_err(),
                "accepted invalid custom source: {input}"
            );
        }
        let too_many_entries = "192.0.2.1\n".repeat(20001);
        assert!(prepare_scan(custom(&too_many_entries, 1))
            .unwrap_err()
            .into_message()
            .contains("20000"));
    }

    #[tokio::test]
    async fn prepared_custom_scan_runs_the_same_candidates_in_the_same_order() {
        let prepared =
            prepare_scan(custom("192.0.2.3\n192.0.2.1\n192.0.2.3\n192.0.2.2", 3)).unwrap();
        let expected = ["192.0.2.3", "192.0.2.1", "192.0.2.2"];
        assert_eq!(
            prepared
                .ips
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            expected
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let outcome = run_prepared_scan(prepared, cancellation, Arc::new(|_| {}))
            .await
            .unwrap();
        assert!(outcome.cancelled);
        assert_eq!(
            outcome
                .results
                .iter()
                .map(|item| item.ip.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(outcome.results.iter().all(|item| {
            item.tcp_ms == 0.0
                && item.attempts == 0
                && item.tcp_stability.is_none()
                && item.download_samples_mbps.is_empty()
        }));
    }

    #[tokio::test]
    async fn prepared_builtin_scan_keeps_its_random_sample_without_resampling() {
        let prepared = prepare_scan(ScanConfig {
            max_candidates: 64,
            ..Default::default()
        })
        .unwrap();
        let expected: Vec<_> = prepared.ips.iter().map(ToString::to_string).collect();
        assert_eq!(expected.len(), 64);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let outcome = run_prepared_scan(prepared, cancellation, Arc::new(|_| {}))
            .await
            .unwrap();
        assert!(outcome.cancelled);
        assert_eq!(
            outcome
                .results
                .iter()
                .map(|item| &item.ip)
                .collect::<Vec<_>>(),
            expected.iter().collect::<Vec<_>>()
        );
        assert!(outcome
            .results
            .iter()
            .all(|item| item.tcp_ms == 0.0 && item.attempts == 0));
    }

    #[test]
    fn targeted_candidates_are_deduplicated_in_original_order_without_sampling() {
        let ips = unique_targeted_ips(vec![
            "192.0.2.3".parse().unwrap(),
            "192.0.2.1".parse().unwrap(),
            "192.0.2.3".parse().unwrap(),
            "192.0.2.2".parse().unwrap(),
        ])
        .unwrap();
        assert_eq!(
            ips,
            ["192.0.2.3", "192.0.2.1", "192.0.2.2"].map(|ip| ip.parse::<Ipv4Addr>().unwrap())
        );
        assert!(unique_targeted_ips(Vec::new()).is_err());
        assert!(unique_targeted_ips((1_u32..=10001).map(Ipv4Addr::from).collect()).is_err());
    }

    #[tokio::test]
    async fn targeted_invalid_input_returns_before_any_measurement_update() {
        let updates = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = updates.clone();
        let callback: UpdateCallback = Arc::new(move |_| {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        assert!(run_targeted_scan(
            ScanConfig::default(),
            Vec::new(),
            CancellationToken::new(),
            callback.clone(),
        )
        .await
        .is_err());
        let invalid = ScanConfig {
            target_url: "http://invalid.example/test".into(),
            ..Default::default()
        };
        assert!(run_targeted_scan(
            invalid,
            vec!["192.0.2.1".parse().unwrap()],
            CancellationToken::new(),
            callback,
        )
        .await
        .is_err());
        assert_eq!(updates.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn targeted_cancellation_preserves_exact_deduplicated_list_without_probing() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let config = ScanConfig {
            max_candidates: 1,
            ..Default::default()
        };
        let outcome = run_targeted_scan(
            config,
            vec![
                "192.0.2.3".parse().unwrap(),
                "192.0.2.1".parse().unwrap(),
                "192.0.2.3".parse().unwrap(),
            ],
            cancellation,
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        assert!(outcome.cancelled);
        assert_eq!(outcome.progress.stage, "cancelled");
        assert_eq!(
            outcome
                .results
                .iter()
                .map(|item| item.ip.as_str())
                .collect::<Vec<_>>(),
            vec!["192.0.2.3", "192.0.2.1"]
        );
        assert!(outcome.results.iter().all(|item| {
            item.attempts == 0
                && item.tcp_stability.is_none()
                && item.download_samples_mbps.is_empty()
                && !item.measurement_passed
        }));
    }

    #[test]
    fn targeted_stability_admits_more_than_fifty_same_region_ips() {
        let config = ScanConfig {
            regions: vec!["HK".into()],
            precision_mode: "limited".into(),
            candidates_per_region: 3,
            ..Default::default()
        };
        let results: Vec<_> = (1..=61)
            .map(|index| measured(&format!("192.0.2.{index}"), "HK", 20.0, 30.0))
            .collect();
        let groups = choose_stability_candidates(&results, &config, ScanScope::Targeted);
        assert_eq!(groups["HK"], (0..61).collect::<Vec<_>>());
        assert_eq!(download_schedule(&groups, &config.regions, 1).len(), 61);
        assert_eq!(choose_per_region(&results, &config)["HK"].len(), 3);
    }

    #[test]
    fn targeted_downloads_admit_every_stable_ip_and_keep_shared_round_schedule() {
        let config = ScanConfig {
            regions: vec!["HK".into()],
            ips_per_region: 1,
            ..Default::default()
        };
        let mut results: Vec<_> = (1..=23)
            .map(|index| measured(&format!("192.0.2.{index}"), "HK", 20.0, 30.0))
            .collect();
        let groups = choose_stability_candidates(&results, &config, ScanScope::Targeted);
        let selected =
            choose_download_candidates(&mut results, &groups, &config, ScanScope::Targeted);
        assert_eq!(selected["HK"].len(), 23);
        let schedule = download_schedule(&selected, &config.regions, config.download_rounds);
        assert_eq!(schedule.len(), 46);
        for index in 0..23 {
            assert_eq!(
                schedule
                    .iter()
                    .filter(|candidate| **candidate == index)
                    .count(),
                2
            );
        }
        assert!(results.iter().all(|item| item.exclusion_reason.is_none()));
    }

    #[test]
    fn targeted_measurement_accepts_known_regions_outside_selection_and_keeps_config() {
        let config = ScanConfig {
            regions: vec!["HK".into()],
            ..Default::default()
        };
        let mut foreign = measured("192.0.2.1", "SE", 20.0, 30.0);
        foreign.colo = "ARN".into();
        foreign.observed_colos = vec!["ARN".into()];
        let mut unknown = IpResult::new("192.0.2.2".parse().unwrap());
        unknown.colo = "ZZZ".into();
        unknown.observed_colos.push("ZZZ".into());
        let mut results = vec![foreign, unknown];
        let effective = measurement_config(&config, &results, ScanScope::Targeted);
        assert_eq!(config.regions, vec!["HK"]);
        assert_eq!(effective.regions, vec!["HK", "SE"]);
        assert!(results[0].has_complete_measurement(&effective));
        assert!(!results[0].has_complete_measurement(&config));
        assert!(!results[1].has_complete_measurement(&effective));
        let groups = choose_stability_candidates(&results, &effective, ScanScope::Targeted);
        assert_eq!(groups["SE"], vec![0]);
        assert!(!groups.contains_key("UNKNOWN"));
        assert_eq!(download_schedule(&groups, &effective.regions, 1), vec![0]);
        let selected =
            choose_download_candidates(&mut results, &groups, &effective, ScanScope::Targeted);
        assert_eq!(selected["SE"], vec![0]);
    }

    #[tokio::test]
    async fn service_verification_stops_at_first_failure_and_preserves_actual_attempts() {
        let mut row = IpResult::new(Ipv4Addr::new(192, 0, 2, 1));
        row.attempts = 1;
        row.success_count = 1;
        observe_colo(&mut row, "HKG");
        let calls = std::cell::Cell::new(0);
        let verified = verify_row(row, vec![70.0], 5, |_| {
            calls.set(calls.get() + 1);
            std::future::ready(Err(ProbeError::new(
                ExclusionCode::HttpsFailure,
                "连接失败",
            )))
        })
        .await;
        assert_eq!(calls.get(), 1, "excluded IP continued service verification");
        assert_eq!(verified.attempts, 2);
        assert_eq!(verified.success_count, 1);
        assert_eq!(verified.https_ms, 70.0);
        assert_eq!(verified.exclusion_code, Some(ExclusionCode::HttpsFailure));
        assert!(verified.exclusion_reason.unwrap().contains("复测失败"));
    }

    #[tokio::test]
    async fn service_verification_stops_after_cross_region_drift_but_allows_same_region_pop_changes(
    ) {
        for (initial, response, expected_calls, excluded) in
            [("HKG", "NRT", 1, true), ("SJC", "LAX", 4, false)]
        {
            let mut row = IpResult::new(Ipv4Addr::new(192, 0, 2, 1));
            row.attempts = 1;
            row.success_count = 1;
            observe_colo(&mut row, initial);
            let calls = std::cell::Cell::new(0);
            let verified = verify_row(row, vec![70.0], 5, |_| {
                calls.set(calls.get() + 1);
                std::future::ready(Ok(TraceProbe {
                    https_ms: 60.0,
                    colo: response.into(),
                }))
            })
            .await;
            assert_eq!(calls.get(), expected_calls);
            assert_eq!(verified.attempts, 1 + expected_calls);
            assert_eq!(verified.success_count, verified.attempts);
            assert_eq!(verified.exclusion_reason.is_some(), excluded);
            assert_eq!(verified.observed_colos, vec![initial, response]);
        }
    }

    #[tokio::test]
    async fn cancelling_service_verification_keeps_completed_observations() {
        let config = ScanConfig {
            regions: vec!["US".into()],
            ..Default::default()
        };
        let ip = Ipv4Addr::new(192, 0, 2, 1);
        let mut row = IpResult::new(ip);
        row.attempts = 1;
        row.success_count = 1;
        row.https_ms = 70.0;
        observe_colo(&mut row, "SJC");
        let mut state = verification_state(vec![row]);
        let cancellation = CancellationToken::new();
        let calls = std::cell::Cell::new(0);
        let callback: UpdateCallback = Arc::new(|_| {});
        let finished = verify_candidates(
            &mut state,
            &[0],
            &[ip],
            &mut BTreeMap::from([(0, vec![70.0])]),
            &config,
            &cancellation,
            &callback,
            |_| {
                let call = calls.get() + 1;
                calls.set(call);
                let cancellation = cancellation.clone();
                async move {
                    if call == 3 {
                        cancellation.cancel();
                        return std::future::pending().await;
                    }
                    Ok(TraceProbe {
                        https_ms: if call == 1 { 20.0 } else { 30.0 },
                        colo: "LAX".into(),
                    })
                }
            },
        )
        .await;
        assert!(!finished);
        assert_eq!(calls.get(), 3);
        assert_eq!(state.progress.completed, 0);
        let outcome = state.cancel(&config, ScanScope::Regional, &callback);
        let saved = &outcome.results[0];
        assert!(outcome.cancelled);
        assert_eq!(saved.attempts, 3, "cancelled request was counted");
        assert_eq!(saved.success_count, 3, "completed successes were discarded");
        assert_eq!(saved.https_ms, 30.0);
        assert_eq!(saved.jitter_ms, 50.0);
        assert_eq!(saved.exclusion_code, Some(ExclusionCode::Cancelled));
        assert_eq!(saved.observed_colos, vec!["SJC", "LAX"]);
        assert!(!saved.measurement_passed);
        assert!(!saved.eligible);
    }

    #[tokio::test]
    async fn service_verification_keeps_per_ip_serial_order_and_bounds_concurrency() {
        use std::cell::{Cell, RefCell};
        let config = ScanConfig {
            tcp_concurrency: 256,
            ..Default::default()
        };
        let ips: Vec<_> = (1..=20)
            .map(|last| Ipv4Addr::new(192, 0, 2, last))
            .collect();
        let rows = ips
            .iter()
            .map(|ip| {
                let mut row = IpResult::new(*ip);
                row.attempts = 1;
                row.success_count = 1;
                observe_colo(&mut row, "HKG");
                row
            })
            .collect();
        let mut state = verification_state(rows);
        let active_ips = RefCell::new(HashSet::new());
        let maximum = Cell::new(0);
        let calls = Cell::new(0);
        assert!(
            verify_candidates(
                &mut state,
                &(0..ips.len()).collect::<Vec<_>>(),
                &ips,
                &mut (0..ips.len()).map(|i| (i, vec![70.0])).collect(),
                &config,
                &CancellationToken::new(),
                &(Arc::new(|_| {}) as UpdateCallback),
                |ip| {
                    let active_ips = &active_ips;
                    let maximum = &maximum;
                    let calls = &calls;
                    async move {
                        assert!(
                            active_ips.borrow_mut().insert(ip),
                            "overlapping requests for one IP"
                        );
                        maximum.set(maximum.get().max(active_ips.borrow().len()));
                        calls.set(calls.get() + 1);
                        tokio::task::yield_now().await;
                        active_ips.borrow_mut().remove(&ip);
                        Ok(TraceProbe {
                            https_ms: 60.0,
                            colo: "HKG".into(),
                        })
                    }
                },
            )
            .await
        );
        assert_eq!(maximum.get(), 16);
        assert_eq!(calls.get(), 20 * (config.verification_attempts - 1));
        assert_eq!(state.progress.completed, 20);
        assert!(state
            .results
            .iter()
            .all(|row| row.attempts == 5 && row.success_count == 5));
    }

    #[tokio::test]
    async fn cancelled_download_pop_recheck_keeps_samples_without_counting_a_failure() {
        let config = ScanConfig::default();
        for prior_rounds in 0..config.download_rounds {
            let mut row = measured("192.0.2.1", "HK", 70.0, 10.0);
            row.download_samples_mbps.truncate(prior_rounds);
            let mut state = verification_state(vec![row]);
            let published = Arc::new(std::sync::Mutex::new(None::<EngineUpdate>));
            let observed = published.clone();
            let callback: UpdateCallback = Arc::new(move |update| {
                *observed.lock().unwrap() = Some(update);
            });
            let mut updates = ProgressUpdates::new(&state, &callback);
            let cancellation = CancellationToken::new();
            assert!(
                !record_download(
                    &mut state,
                    0,
                    DownloadProbe {
                        mbps: 30.0,
                        colo: None
                    },
                    &config,
                    &cancellation,
                    &mut updates,
                    || async {
                        {
                            let event = published.lock().unwrap();
                            let update =
                                event.as_ref().expect("sample published before POP recheck");
                            let sample = &update.changed_rows[&0];
                            assert_eq!(sample.download_samples_mbps.last(), Some(&30.0));
                            assert_eq!(
                                sample.exclusion_code,
                                Some(ExclusionCode::PopRecheckIncomplete)
                            );
                            assert!(!sample.measurement_passed);
                        }
                        cancellation.cancel();
                        std::future::pending().await
                    },
                )
                .await
            );
            let outcome = state.cancel(
                &config,
                ScanScope::Regional,
                &(Arc::new(|_| {}) as UpdateCallback),
            );
            let saved = &outcome.results[0];
            assert_eq!(saved.attempts, 5);
            assert_eq!(saved.success_count, 5);
            assert_eq!(saved.download_samples_mbps.len(), prior_rounds + 1);
            assert_eq!(saved.download_samples_mbps.last(), Some(&30.0));
            assert_eq!(
                saved.download_mbps,
                if prior_rounds == 0 { 30.0 } else { 20.0 }
            );
            assert_eq!(
                saved.exclusion_reason.as_deref(),
                Some("下载后的 POP 复核未完成")
            );
            assert_eq!(
                saved.exclusion_code,
                Some(ExclusionCode::PopRecheckIncomplete)
            );
            assert_eq!(saved.download_pop_complete(false), Some(false));
            assert!(!saved.has_complete_measurement(&config));
            assert!(
                !saved.measurement_passed,
                "final-round cancellation passed measurement"
            );
            assert!(!saved.eligible);
        }
    }

    #[tokio::test]
    async fn download_pop_recheck_records_success_failure_and_drift() {
        let config = ScanConfig::default();
        for response in [Ok("HKG"), Ok("NRT"), Err("TLS 验证失败")] {
            let mut row = measured("192.0.2.1", "HK", 70.0, 10.0);
            row.download_samples_mbps.truncate(1);
            let mut state = verification_state(vec![row]);
            let callback: UpdateCallback = Arc::new(|_| {});
            let mut updates = ProgressUpdates::new(&state, &callback);
            assert!(
                record_download(
                    &mut state,
                    0,
                    DownloadProbe {
                        mbps: 30.0,
                        colo: None
                    },
                    &config,
                    &CancellationToken::new(),
                    &mut updates,
                    || std::future::ready(
                        response
                            .map(|colo| TraceProbe {
                                https_ms: 60.0,
                                colo: colo.into()
                            })
                            .map_err(|message| ProbeError::new(
                                ExclusionCode::HttpsFailure,
                                message
                            ))
                    ),
                )
                .await
            );
            let row = &mut state.results[0];
            assert_eq!(row.attempts, 6);
            assert_eq!(row.success_count, if response.is_ok() { 6 } else { 5 });
            assert_eq!(row.download_samples_mbps, vec![10.0, 30.0]);
            assert_eq!(row.download_mbps, 20.0);
            rank_results(std::slice::from_mut(row), &config, true);
            let passed = response == Ok("HKG");
            assert_eq!(row.measurement_passed, passed);
            assert_eq!(row.eligible, passed);
            assert_eq!(row.exclusion_reason.is_none(), passed);
            let code = match response {
                Ok("HKG") => None,
                Ok(_) => Some(ExclusionCode::PopDrift),
                Err(_) => Some(ExclusionCode::PopRecheckFailure),
            };
            assert_eq!(row.exclusion_code, code);
            assert_eq!(row.download_pop_complete(false), Some(response.is_ok()));
        }
    }

    #[tokio::test]
    async fn download_with_cf_ray_needs_no_additional_trace_request() {
        let config = ScanConfig::default();
        let mut row = measured("192.0.2.1", "HK", 70.0, 10.0);
        row.download_samples_mbps.truncate(1);
        let mut state = verification_state(vec![row]);
        let callback: UpdateCallback = Arc::new(|_| {});
        let mut updates = ProgressUpdates::new(&state, &callback);
        let calls = std::cell::Cell::new(0);
        assert!(
            record_download(
                &mut state,
                0,
                DownloadProbe {
                    mbps: 30.0,
                    colo: Some("HKG".into())
                },
                &config,
                &CancellationToken::new(),
                &mut updates,
                || {
                    calls.set(calls.get() + 1);
                    std::future::ready(Err(ProbeError::new(
                        ExclusionCode::HttpsFailure,
                        "unnecessary trace request",
                    )))
                },
            )
            .await
        );
        let row = &state.results[0];
        assert_eq!(calls.get(), 0);
        assert_eq!(row.attempts, 5);
        assert_eq!(row.success_count, 5);
        assert_eq!(row.download_samples_mbps, vec![10.0, 30.0]);
        assert!(row.measurement_passed);
    }

    #[test]
    fn builtin_default_samples_exactly_one_host_from_every_cloudflare_prefix() {
        let samples = sample_candidates(&ScanConfig::default()).unwrap();
        assert_eq!(samples.len(), crate::model::CLOUDFLARE_CANDIDATE_COUNT);
        assert_eq!(samples.iter().collect::<HashSet<_>>().len(), samples.len());
        let actual_prefixes: HashSet<u32> = samples.iter().map(|ip| u32::from(*ip) >> 8).collect();
        let mut expected_prefixes = HashSet::new();
        for cidr in CLOUDFLARE_IPV4 {
            let net: Ipv4Net = cidr.parse().unwrap();
            let first_prefix = u32::from(net.network()) >> 8;
            for offset in 0..(1_u32 << (24 - net.prefix_len())) {
                expected_prefixes.insert(first_prefix + offset);
            }
        }
        assert_eq!(actual_prefixes, expected_prefixes);
        assert_eq!(actual_prefixes.len(), samples.len());
        assert!(samples.iter().all(|ip| (1..=254).contains(&ip.octets()[3])));
    }

    #[test]
    fn builtin_allocates_candidates_by_prefix_capacity_not_equal_cidr_counts() {
        let samples = sample_cloudflare(1024).unwrap();
        let networks: Vec<Ipv4Net> = CLOUDFLARE_IPV4
            .iter()
            .map(|cidr| cidr.parse().unwrap())
            .collect();
        let total_prefixes: usize = networks
            .iter()
            .map(|net| 1_usize << (24 - net.prefix_len()))
            .sum();
        for network in networks {
            let count = samples.iter().filter(|ip| network.contains(*ip)).count();
            let prefixes = 1_usize << (24 - network.prefix_len());
            let expected = 1024.0 * prefixes as f64 / total_prefixes as f64;
            assert!(
                (count as f64 - expected).abs() <= 15.0,
                "{network}: {count} versus {expected}"
            );
            if network.prefix_len() == 22 {
                assert!(count <= 2);
            }
            if network.prefix_len() == 13 {
                assert!(count > 300);
            }
        }
    }

    #[test]
    fn builtin_sampling_bounds_cover_single_candidate_and_two_prefix_rounds() {
        assert!(sample_cloudflare(0).is_err());
        assert_eq!(sample_cloudflare(1).unwrap().len(), 1);
        let samples = sample_cloudflare(10000).unwrap();
        assert_eq!(samples.len(), 10000);
        assert_eq!(samples.iter().collect::<HashSet<_>>().len(), 10000);
        let mut prefixes = BTreeMap::<u32, usize>::new();
        for ip in samples {
            *prefixes.entry(u32::from(ip) >> 8).or_default() += 1;
            assert!(CLOUDFLARE_IPV4
                .iter()
                .any(|cidr| cidr.parse::<Ipv4Net>().unwrap().contains(&ip)));
        }
        assert_eq!(prefixes.len(), 5956);
        assert!(prefixes.values().all(|count| *count <= 2));
    }

    #[test]
    fn region_precision_quota_keeps_fast_same_prefix_hosts_ahead_of_slower_prefixes() {
        let results = vec![
            measured("192.0.2.1", "HK", 10.0, 8.0),
            measured("192.0.2.2", "HK", 20.0, 8.0),
            measured("192.0.3.1", "HK", 30.0, 8.0),
        ];
        let config = ScanConfig {
            ips_per_region: 1,
            precision_mode: "limited".into(),
            candidates_per_region: 2,
            ..Default::default()
        };
        assert_eq!(choose_per_region(&results, &config)["HK"], vec![0, 1]);
    }

    #[test]
    fn regional_latency_queue_caps_each_region_independently_and_never_refills_beyond_twenty() {
        let config = ScanConfig {
            regions: vec!["HK".into(), "US".into()],
            ..Default::default()
        };
        // Reverse insertion order and same-prefix hosts reproduce the former
        // diversity reorder while ensuring selection follows TCP latency alone.
        let mut results: Vec<_> = (0..100)
            .rev()
            .map(|index| {
                let mut item = measured(
                    &format!("192.0.2.{}", index + 1),
                    "US",
                    400.0 + index as f64,
                    8.0,
                );
                item.eligible = false;
                item
            })
            .collect();
        results.extend((0..25).rev().map(|index| {
            measured(
                &format!("198.51.100.{}", index + 1),
                "HK",
                100.0 + index as f64,
                8.0,
            )
        }));
        let groups = choose_per_region(&results, &config);
        assert_eq!(groups["US"].len(), 20);
        assert_eq!(groups["HK"].len(), 20);
        assert_eq!(groups["US"], (80..100).rev().collect::<Vec<_>>());
        assert_eq!(groups["HK"], (105..125).rev().collect::<Vec<_>>());
        assert!(groups["US"].iter().all(|index| *index >= 80));
        assert!(!groups.values().flatten().any(|index| *index < 80));
    }

    #[test]
    fn manual_precision_cap_cannot_expand_the_regional_latency_queue() {
        let results: Vec<_> = (0..60)
            .map(|index| {
                measured(
                    &format!("192.0.2.{}", index + 1),
                    "US",
                    100.0 + index as f64,
                    8.0,
                )
            })
            .collect();
        for (manual_cap, expected) in [(10, 10), (80, 20)] {
            let config = ScanConfig {
                regions: vec!["US".into()],
                precision_mode: "limited".into(),
                candidates_per_region: manual_cap,
                ..Default::default()
            };
            assert_eq!(choose_per_region(&results, &config)["US"].len(), expected);
        }
    }

    #[test]
    fn downloads_choose_the_best_ten_after_the_entire_regional_pool_is_measured() {
        let config = ScanConfig {
            regions: vec!["HK".into()],
            ips_per_region: 1,
            preference: "download".into(),
            ..Default::default()
        };
        let mut results: Vec<_> = (0..20)
            .map(|index| {
                let mut item = measured(
                    &format!("192.0.2.{}", index + 1),
                    "HK",
                    10.0 + index as f64,
                    0.0,
                );
                item.attempts = 1;
                item.success_count = 1;
                item.download_samples_mbps.clear();
                // The best full measurement arrives at the end of the initial queue.
                item.tcp_stability = Some(crate::stability::summarize(
                    100,
                    230.0,
                    vec![Some(200.0 - index as f64); 100],
                ));
                item
            })
            .collect();
        let groups = choose_per_region(&results, &config);
        assert_eq!(groups["HK"], (0..20).collect::<Vec<_>>());
        let shortlist = choose_download_per_region(&mut results, &groups, &config);
        assert_eq!(shortlist["HK"], (10..20).rev().collect::<Vec<_>>());
        assert_eq!(download_schedule(&shortlist, &config.regions, 2).len(), 20);
        assert!(results[..10].iter().all(|row| row
            .exclusion_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("未进入本地区前 10 名下载候选"))));
        assert!(results
            .iter()
            .all(|row| row.tcp_stability.as_ref().unwrap().completed));
        assert!(results
            .iter()
            .all(|row| row.download_samples_mbps.is_empty()));
        assert!(results
            .iter()
            .all(|row| !row.measurement_passed && !row.eligible));
    }

    #[test]
    fn download_shortlist_is_per_region_and_excludes_failed_or_incomplete_stability() {
        let config = ScanConfig {
            regions: vec!["HK".into(), "JP".into()],
            max_stability_failure_percent: 100.0,
            ..Default::default()
        };
        let mut results: Vec<_> = (0..30)
            .map(|index| {
                measured(
                    &format!("192.0.2.{}", index + 1),
                    if index < 15 { "HK" } else { "JP" },
                    20.0 + index as f64,
                    0.0,
                )
            })
            .collect();
        let mut failures = vec![Some(1.0); 98];
        failures.extend([None, None]);
        results[15].tcp_stability = Some(crate::stability::summarize(100, 230.0, failures));
        results[16].tcp_stability =
            Some(crate::stability::summarize(100, 230.0, vec![Some(1.0); 99]));
        results[17].tcp_stability = None;
        results[18].tcp_stability = Some(crate::stability::summarize(
            100,
            230.0,
            vec![Some(231.0); 100],
        ));
        results[19].exclusion_reason = Some("接入地区发生变化".into());
        results[20].tcp_stability = Some(crate::stability::summarize(100, 230.0, vec![None, None]));
        results[20].tcp_stability.as_mut().unwrap().early_rejection = Some("stop".into());
        let groups = choose_per_region(&results, &config);
        let shortlist = choose_download_per_region(&mut results, &groups, &config);
        assert_eq!(shortlist["HK"], (0..10).collect::<Vec<_>>());
        assert_eq!(shortlist["JP"], (21..30).collect::<Vec<_>>());
        assert_eq!(shortlist.values().map(Vec::len).sum::<usize>(), 19);
        assert!(!shortlist
            .values()
            .flatten()
            .any(|index| (15..=20).contains(index)));
    }

    #[test]
    fn download_shortlist_ranks_failure_rate_then_the_configured_full_latency_metric() {
        let mut results = vec![
            measured("192.0.2.1", "HK", 40.0, 0.0),
            measured("192.0.2.2", "HK", 40.0, 0.0),
            measured("192.0.2.3", "HK", 40.0, 0.0),
        ];
        let mut peaks = vec![Some(10.0); 95];
        peaks.extend([Some(1000.0); 5]);
        results[0].tcp_stability = Some(crate::stability::summarize(100, 230.0, peaks));
        results[1].tcp_stability = Some(crate::stability::summarize(
            100,
            230.0,
            vec![Some(30.0); 100],
        ));
        let mut unreliable = vec![Some(1.0); 99];
        unreliable.push(None);
        results[2].tcp_stability = Some(crate::stability::summarize(100, 230.0, unreliable));
        let config = ScanConfig::default();
        let groups = choose_per_region(&results, &config);
        assert_eq!(
            choose_download_per_region(&mut results, &groups, &config)["HK"],
            vec![0, 1, 2]
        );
        let average = ScanConfig {
            stability_latency_metric: "average".into(),
            ..config
        };
        assert_eq!(
            choose_download_per_region(&mut results, &groups, &average)["HK"],
            vec![1, 0, 2]
        );
    }

    #[test]
    fn download_budget_is_shared_across_the_final_shortlist_and_never_reset() {
        let total = 400 * 1024 * 1024;
        let mut used = 0;
        for remaining in (1..=40).rev() {
            let allowance = download_allowance(total, used, remaining);
            assert_eq!(allowance, 10 * 1024 * 1024);
            assert!(allowance <= total - used);
            used += allowance;
        }
        assert_eq!(used, total);
        assert_eq!(download_allowance(total, total, 1), 0);
        assert_eq!(download_allowance(total, total + 1, 1), 0);
    }

    #[tokio::test]
    async fn download_waiting_for_headers_uses_only_the_request_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_http_fixture_headers(&mut socket).await.unwrap();
            std::future::pending::<()>().await;
        });
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let config = ScanConfig {
            request_timeout_ms: 100,
            download_seconds: 1,
            ..Default::default()
        };
        let error = tokio::time::timeout(
            Duration::from_millis(800),
            measure_download_request(
                &config,
                Client::new().get(format!("http://{address}/download")),
                Ipv4Addr::LOCALHOST,
                None,
                counter.clone(),
            ),
        )
        .await
        .expect("header timeout must fire before the body window")
        .err()
        .unwrap();
        server.abort();
        assert_eq!(error.code, ExclusionCode::HttpsTimeout);
        assert_eq!(error.message, "等待下载响应头超时");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn unlimited_downloads_read_slow_unknown_lengths_until_the_independent_body_deadline() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_http_fixture_headers(&mut socket).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nCF-Ray: abc-HKG\r\n\r\n")
                .await
                .unwrap();
            // The response header is quick, but the body arrives after the
            // header timeout and then stays open past the body deadline.
            tokio::time::sleep(Duration::from_millis(400)).await;
            socket.write_all(&vec![b'x'; 128 * 1024]).await.unwrap();
            std::future::pending::<()>().await;
        });
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let config = ScanConfig {
            request_timeout_ms: 250,
            download_budget_enabled: false,
            download_budget_mi_b: 0,
            download_seconds: 1,
            ..Default::default()
        };
        let started = Instant::now();
        let sample = tokio::time::timeout(
            Duration::from_secs(2),
            measure_download_request(
                &config,
                Client::new().get(format!("http://{address}/download")),
                Ipv4Addr::LOCALHOST,
                None,
                counter.clone(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            128 * 1024
        );
        assert!(sample.mbps > 0.0);
        assert_eq!(sample.colo.as_deref(), Some("HKG"));
    }

    #[tokio::test]
    async fn budgeted_download_rejects_an_unknown_length_before_reading_the_body() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_http_fixture_headers(&mut socket).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let response = Client::new()
            .get(format!("http://{address}/download"))
            .send()
            .await
            .unwrap();
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let config = ScanConfig {
            download_budget_enabled: true,
            ..Default::default()
        };
        let error = measure_download_response(
            &config,
            response,
            Ipv4Addr::LOCALHOST,
            Some(MIN_SAMPLE_BYTES),
            Instant::now(),
            counter.clone(),
        )
        .await
        .err()
        .unwrap();
        server.abort();
        assert_eq!(error.code, ExclusionCode::DownloadFailure);
        assert!(error.message.contains("预算内的明确内容长度"));
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn low_budget_preserves_complete_samples_across_regions() {
        let total = 1024 * 1024;
        let groups = BTreeMap::from([
            ("HK".into(), (0..10).collect()),
            ("JP".into(), (10..20).collect()),
            ("SG".into(), (20..30).collect()),
            ("US".into(), (30..40).collect()),
        ]);
        let order = vec!["HK".into(), "JP".into(), "SG".into(), "US".into()];
        for (rounds, expected, unfunded) in [
            (
                2,
                vec![0, 10, 20, 30, 0, 10, 20, 30, 1, 11, 21, 31, 1, 11, 21, 31],
                32,
            ),
            (5, [0, 10, 20].repeat(5), 37),
        ] {
            let mut queue = DownloadQueue::new(&groups, &order, rounds);
            let mut samples = Vec::new();
            let mut skipped = 0;
            let mut used = 0;
            while let Some(slot) = queue.next(Some(total), used) {
                match slot {
                    DownloadSlot::Round { index, allowance } => {
                        assert_eq!(allowance, Some(MIN_SAMPLE_BYTES));
                        used += allowance.unwrap();
                        samples.push(index);
                    }
                    DownloadSlot::Unfunded(_) => skipped += 1,
                }
            }
            assert_eq!(samples, expected);
            assert_eq!(skipped, unfunded);
            assert_eq!(used, samples.len() as u64 * MIN_SAMPLE_BYTES);
            assert!(used <= total);
            assert_eq!(queue.remaining_rounds, 0);
        }
        assert_eq!(download_allowance(total, total - 1, 80), 1);
    }

    #[test]
    fn a_failed_download_frees_the_reserved_rounds_for_the_next_region() {
        let order: Vec<_> = ["HK", "JP", "SG", "US"].map(str::to_owned).into();
        let groups = order
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, region)| (region, vec![index]))
            .collect();
        let mut queue = DownloadQueue::new(&groups, &order, 5);
        let total = 1024 * 1024;
        assert!(matches!(
            queue.next(Some(total), 0),
            Some(DownloadSlot::Round { index: 0, .. })
        ));
        assert_eq!(queue.skip_remaining_rounds(0), 4);

        let mut used = 0;
        let mut samples = Vec::new();
        while let Some(slot) = queue.next(Some(total), used) {
            let DownloadSlot::Round {
                index,
                allowance: Some(bytes),
            } = slot
            else {
                panic!("remaining regions should all be funded");
            };
            assert!(bytes >= MIN_SAMPLE_BYTES);
            used += bytes;
            samples.push(index);
        }
        assert_eq!(samples, [[1, 2].repeat(5), vec![3; 5]].concat());
        assert_eq!(used, total);
    }

    #[test]
    fn huge_cidr_materializes_only_the_limit() {
        let started = Instant::now();
        let samples = sample_candidates(&custom(
            "128.0.0.0/1\n0.0.0.0/0\n255.255.255.255\n0.0.0.0",
            128,
        ))
        .unwrap();
        assert_eq!(samples.len(), 128);
        assert_eq!(&samples[..2], &[Ipv4Addr::BROADCAST, Ipv4Addr::UNSPECIFIED]);
        assert_eq!(samples.iter().collect::<HashSet<_>>().len(), 128);
        assert!(samples[2..].iter().any(|ip| u32::from(*ip) < 1 << 31));
        assert!(samples[2..].iter().any(|ip| u32::from(*ip) >= 1 << 31));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn deduplicates_and_handles_singletons_and_overlaps() {
        let samples = sample_candidates(&custom(
            "# comment\n192.0.2.1, 192.0.2.1; 192.0.2.0/31\n192.0.2.1/32",
            100,
        ))
        .unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0], "192.0.2.1".parse::<Ipv4Addr>().unwrap());
        assert!(samples.contains(&"192.0.2.0".parse::<Ipv4Addr>().unwrap()));
    }

    #[test]
    fn dense_contained_networks_do_not_consume_the_larger_networks_quota() {
        let base = u32::from(Ipv4Addr::new(10, 0, 0, 0));
        let source = (0..4096)
            .map(|offset| format!("{}/32", Ipv4Addr::from(base + offset)))
            .chain(
                (0..4096)
                    .step_by(2)
                    .map(|offset| format!("{}/31", Ipv4Addr::from(base + offset))),
            )
            .chain(["10.0.3.1/16".into()])
            .collect::<Vec<_>>()
            .join("\n");
        let samples = sample_candidates(&custom(&source, 10000)).unwrap();
        assert_eq!(samples.len(), 10000);
        assert_eq!(samples.iter().collect::<HashSet<_>>().len(), 10000);
        assert!(samples.iter().all(|ip| ip.octets()[..2] == [10, 0]));
        assert!(samples.iter().any(|ip| ip.octets()[2] >= 16));
    }

    #[test]
    fn custom_sampling_prioritizes_explicit_ips_and_redistributes_exhausted_networks() {
        let source = "198.51.100.255/32\n192.0.2.0/30\n255.255.255.254/31\n192.0.2.1\n255.255.255.255\n192.0.2.0\n192.0.2.1";
        let expected: Vec<Ipv4Addr> = [
            "192.0.2.1",
            "255.255.255.255",
            "192.0.2.0",
            "198.51.100.255",
            "192.0.2.2",
            "255.255.255.254",
            "192.0.2.3",
        ]
        .map(|ip| ip.parse().unwrap())
        .into();
        assert_eq!(
            sample_candidates(&custom(source, 2)).unwrap(),
            expected[..2]
        );
        assert_eq!(sample_candidates(&custom(source, 100)).unwrap(), expected);
    }

    #[test]
    fn invalid_custom_sampling_limits_return_an_error() {
        for limit in [0, 10001] {
            assert!(sample_candidates(&custom("192.0.2.1", limit)).is_err());
        }
    }

    #[test]
    fn malformed_custom_entries_fail_instead_of_being_silently_ignored() {
        for input in [
            "not an IP",
            "1.2.3.999",
            "2001:db8::1",
            "192.0.2.0/33",
            "# empty",
        ] {
            assert!(
                sample_candidates(&custom(input, 128)).is_err(),
                "accepted {input}"
            );
        }
    }

    #[test]
    fn trace_requires_actual_pop_and_correct_target_host() {
        let target = Url::parse("https://speed.cloudflare.com/__down?bytes=1000000").unwrap();
        assert_eq!(
            parse_trace("h=speed.cloudflare.com\ncolo=NRT\nloc=CN\n", &target).unwrap(),
            "NRT"
        );
        // loc is the client location, never the edge POP's region.
        assert!(parse_trace("h=speed.cloudflare.com\nloc=JP\n", &target).is_err());
        assert!(parse_trace("h=other.example\ncolo=NRT\n", &target).is_err());
        assert!(parse_trace(
            "h=other.example\nh=speed.cloudflare.com\ncolo=NRT\n",
            &target
        )
        .is_err());
        assert!(parse_trace("h=speed.cloudflare.com\ncolo=NRT\ncolo=HKG", &target).is_err());
        assert!(parse_trace("<html>colo=NRT</html>", &target).is_err());
        assert_eq!(
            parse_trace("h=speed.cloudflare.com\ncolo=ZZZ", &target).unwrap(),
            "ZZZ"
        );
    }

    #[test]
    fn region_quotas_do_not_discard_distant_regions() {
        let results = vec![
            measured("192.0.2.1", "HK", 10.0, 20.0),
            measured("192.0.2.2", "HK", 20.0, 20.0),
            measured("192.0.2.3", "JP", 50.0, 20.0),
            measured("192.0.2.4", "US", 200.0, 20.0),
        ];
        let config = ScanConfig {
            regions: vec!["HK".into(), "JP".into(), "US".into()],
            ips_per_region: 1,
            precision_mode: "limited".into(),
            candidates_per_region: 1,
            ..Default::default()
        };
        let groups = choose_per_region(&results, &config);
        assert_eq!(groups["HK"], vec![0]);
        assert_eq!(groups["JP"], vec![2]);
        assert_eq!(groups["US"], vec![3]);
    }

    #[test]
    fn country_drift_is_excluded_but_same_country_pop_changes_are_retained() {
        let mut item = IpResult::new("192.0.2.1".parse().unwrap());
        observe_colo(&mut item, "NRT");
        observe_colo(&mut item, "KIX");
        assert_eq!(item.region_code, "JP");
        assert!(item.exclusion_reason.is_none());
        observe_colo(&mut item, "HKG");
        assert!(item
            .exclusion_reason
            .as_ref()
            .unwrap()
            .contains("接入地区发生变化"));
        assert_eq!(item.observed_colos, vec!["NRT", "KIX", "HKG"]);
        let mut unknown = IpResult::new("192.0.2.2".parse().unwrap());
        observe_colo(&mut unknown, "ZZZ");
        assert_eq!(unknown.region_code, "UNKNOWN");
    }

    #[test]
    fn daily_mode_prefers_low_latency_after_a_bandwidth_floor() {
        let mut results = vec![
            measured("192.0.2.1", "HK", 10.0, 30.0),
            measured("192.0.2.2", "HK", 20.0, 100.0),
            measured("192.0.2.3", "HK", 5.0, 5.0),
        ];
        rank_results(&mut results, &ScanConfig::default(), true);
        assert_eq!(results[0].role.as_deref(), Some("primary"));
        assert_eq!(results[1].role.as_deref(), Some("backup"));
        assert!(!results[2].eligible);
        assert!(results[2].measurement_passed);
        assert!(results[2]
            .exclusion_reason
            .as_ref()
            .unwrap()
            .contains("低于"));
    }

    #[test]
    fn historical_single_probe_gate_includes_200_but_rejects_200_point_001() {
        let config = ScanConfig {
            tcp_latency_screening: true,
            max_tcp_ms: Some(200.0),
            ..Default::default()
        };
        let mut boundary = IpResult::new("192.0.2.1".parse().unwrap());
        apply_tcp_measurement(&mut boundary, 200.0, &config);
        assert!(boundary.exclusion_reason.is_none());
        let mut over = IpResult::new("192.0.2.2".parse().unwrap());
        apply_tcp_measurement(&mut over, 200.001, &config);
        assert_eq!(over.tcp_ms, 200.001);
        assert!(over
            .exclusion_reason
            .as_deref()
            .unwrap()
            .starts_with("TCP 延迟 "));
        assert_eq!(over.attempts, 0);
        assert!(over.colo.is_empty());
        assert!(!over.measurement_passed && !over.eligible);
    }

    #[test]
    fn historical_high_speed_candidate_above_initial_tcp_gate_is_never_recommended() {
        let config = ScanConfig {
            tcp_latency_screening: true,
            max_tcp_ms: Some(200.0),
            ..Default::default()
        };
        let mut boundary = measured("192.0.2.1", "HK", 500.0, 500.0);
        boundary.tcp_ms = 200.0;
        boundary.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(200.0); 100],
        ));
        let mut over = boundary.clone();
        over.ip = "192.0.2.2".into();
        over.tcp_ms = 200.001;
        let mut results = vec![over, boundary];
        rank_results(&mut results, &config, true);
        assert!(!results[0].measurement_passed && !results[0].eligible);
        assert_eq!(results[0].role.as_deref(), Some("candidate"));
        assert!(results[1].measurement_passed && results[1].eligible);
        assert_eq!(results[1].role.as_deref(), Some("primary"));
    }

    #[test]
    fn new_screening_sends_slow_successful_connections_to_pop_discovery() {
        let config = ScanConfig::default();
        let ips = vec![
            Ipv4Addr::new(192, 0, 2, 1),
            Ipv4Addr::new(192, 0, 2, 2),
            Ipv4Addr::new(192, 0, 2, 3),
        ];
        let mut results: Vec<_> = ips.iter().copied().map(IpResult::new).collect();
        apply_tcp_measurement(&mut results[0], 350.0, &config);
        apply_tcp_measurement(&mut results[1], 600.0, &config);
        results[2].exclusion_reason = Some("TCP 连接超时".into());
        assert_eq!(
            pop_discovery_candidates(&results, &ips),
            vec![(0, ips[0]), (1, ips[1])]
        );
        assert_eq!(results[0].tcp_ms, 350.0);
        assert_eq!(results[1].tcp_ms, 600.0);
    }

    #[test]
    fn high_initial_latency_is_not_a_hidden_final_gate_after_full_stability_testing() {
        let config = ScanConfig {
            max_tcp_ms: Some(200.0),
            ..Default::default()
        };
        let mut row = measured("192.0.2.1", "HK", 40.0, 30.0);
        row.tcp_ms = 350.0;
        let mut probes = vec![Some(180.0); 99];
        probes.push(None);
        row.tcp_stability = Some(crate::stability::summarize(100, 200.0, probes));
        let mut rows = vec![row.clone()];
        rank_results(&mut rows, &config, true);
        assert!(rows[0].eligible && rows[0].measurement_passed);
        assert_eq!(rows[0].role.as_deref(), Some("primary"));
        assert_eq!(rows[0].tcp_ms, 350.0);
        assert_eq!(crate::model::tcp_display_ms(&rows[0]), 180.0);
        row.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(200.0); 100],
        ));
        assert!(row.has_complete_measurement(&config));
        row.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(200.001); 100],
        ));
        assert!(!row.has_complete_measurement(&config));
        let mut failed = vec![Some(180.0); 98];
        failed.extend([None, None]);
        row.tcp_stability = Some(crate::stability::summarize(100, 200.0, failed));
        assert!(!row.has_complete_measurement(&config));
        row.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(180.0); 99],
        ));
        assert!(!row.has_complete_measurement(&config));
    }

    #[test]
    fn tcp_orders_precision_and_daily_recommendations_even_when_https_is_slower() {
        let config = ScanConfig {
            ips_per_region: 1,
            precision_mode: "limited".into(),
            candidates_per_region: 1,
            ..Default::default()
        };
        let mut low_tcp = measured("192.0.2.1", "HK", 600.0, 30.0);
        low_tcp.tcp_ms = 50.0;
        low_tcp.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(50.0); 100],
        ));
        let mut low_https = measured("192.0.3.1", "HK", 100.0, 100.0);
        low_https.tcp_ms = 100.0;
        low_https.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(100.0); 100],
        ));
        let mut results = vec![low_https, low_tcp];
        assert_eq!(choose_per_region(&results, &config)["HK"], vec![1]);
        rank_results(&mut results, &config, true);
        assert_eq!(results[1].role.as_deref(), Some("primary"));
        let config = ScanConfig {
            preference: "download".into(),
            ..Default::default()
        };
        rank_results(&mut results, &config, true);
        assert_eq!(results[0].role.as_deref(), Some("primary"));
        results[1].download_mbps = results[0].download_mbps;
        rank_results(&mut results, &config, true);
        assert_eq!(results[1].role.as_deref(), Some("primary"));
    }

    #[test]
    fn download_mode_prefers_throughput_and_never_promotes_incomplete_or_failed_candidates() {
        let mut results = vec![
            measured("192.0.2.1", "HK", 10.0, 30.0),
            measured("192.0.2.2", "HK", 20.0, 100.0),
            measured("192.0.2.3", "HK", 5.0, 500.0),
            measured("192.0.2.4", "HK", 4.0, 600.0),
        ];
        results[2].download_samples_mbps.pop();
        results[3].success_count = 4;
        let config = ScanConfig {
            preference: "download".into(),
            ..Default::default()
        };
        rank_results(&mut results, &config, true);
        assert_eq!(results[1].role.as_deref(), Some("primary"));
        assert!(!results[2].eligible);
        assert!(!results[3].eligible);
        assert!(!results[2].measurement_passed);
        assert!(!results[3].measurement_passed);
        rank_results(&mut results, &config, false);
        assert!(results
            .iter()
            .all(|r| r.role.as_deref() == Some("candidate")));
        assert!(results[1].eligible);
    }

    #[test]
    fn repeated_ranking_keeps_speed_filtered_measurements_verified() {
        let config = ScanConfig::default();
        let mut results = vec![measured("192.0.2.1", "HK", 200.0, 8.0)];
        rank_results(&mut results, &config, true);
        rank_results(&mut results, &config, false);
        assert!(results[0].measurement_passed);
        assert!(!results[0].eligible);
        assert!(results[0].has_only_speed_target_exclusion());
        assert_eq!(results[0].exclusion_code, Some(ExclusionCode::SpeedTarget));
        assert_eq!(results[0].role.as_deref(), Some("candidate"));
        rank_results(
            &mut results,
            &ScanConfig {
                min_download_mbps: 0.0,
                ..config
            },
            true,
        );
        assert!(results[0].eligible);
        assert!(!results[0].is_excluded());
    }

    #[test]
    fn complete_samples_cannot_override_a_hard_failure_or_country_drift() {
        let config = ScanConfig::default();
        let mut hard = measured("192.0.2.1", "HK", 200.0, 8.0);
        hard.exclusion_reason = Some("下载后的 POP 复核失败：HTTPS 请求超时".into());
        let mut drift = measured("192.0.2.2", "HK", 200.0, 8.0);
        drift.observed_colos.push("NRT".into());
        let mut results = vec![hard, drift];
        rank_results(&mut results, &config, true);
        assert!(results
            .iter()
            .all(|result| !result.measurement_passed && !result.eligible));
    }

    #[test]
    fn stability_gate_allows_one_failure_and_uses_p95_instead_of_one_peak() {
        let config = ScanConfig::default();
        let mut row = measured("192.0.2.1", "HK", 40.0, 30.0);
        let mut probes = vec![Some(20.0); 100];
        probes[20] = None;
        row.tcp_stability = Some(crate::stability::summarize(100, 200.0, probes.clone()));
        assert!(row.stability_exclusion(&config).is_none());
        assert!(row.has_complete_measurement(&config));
        probes[21] = None;
        row.tcp_stability = Some(crate::stability::summarize(100, 200.0, probes));
        assert!(row
            .stability_exclusion(&config)
            .unwrap()
            .contains("累计 2 次连接失败"));
        let mut probes = vec![Some(20.0); 100];
        probes[99] = Some(200.001);
        row.tcp_stability = Some(crate::stability::summarize(100, 200.0, probes));
        assert!(row.tcp_stability.as_ref().unwrap().average_ms.unwrap() < 200.0);
        assert_eq!(row.tcp_stability.as_ref().unwrap().over_limit_count, 1);
        assert!(row.stability_exclusion(&config).is_none());
        let strict = ScanConfig {
            stability_latency_metric: "max".into(),
            max_tcp_ms: Some(200.0),
            ..Default::default()
        };
        assert!(row
            .stability_exclusion(&strict)
            .unwrap()
            .contains("200.001"));
        let mut rows = vec![row];
        rank_results(&mut rows, &config, true);
        assert!(rows[0].measurement_passed && rows[0].eligible);
    }

    #[test]
    fn stability_gate_requires_complete_results_and_consistent_display_aggregates() {
        let config = ScanConfig::default();
        let mut row = measured("192.0.2.1", "HK", 40.0, 30.0);
        row.tcp_stability = None;
        assert!(row.stability_exclusion(&config).unwrap().contains("未完成"));
        row.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(20.0); 99],
        ));
        assert!(!row.has_complete_measurement(&config));
        row.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(20.0); 100],
        ));
        row.tcp_stability.as_mut().unwrap().average_ms = Some(f64::NAN);
        assert!(!row.has_complete_measurement(&config));
        row.tcp_stability = None;
        let legacy = ScanConfig {
            stability_attempts: None,
            ..Default::default()
        };
        assert!(row.has_complete_measurement(&legacy));
        assert!(!row.has_complete_measurement(&config));
    }

    #[test]
    fn completed_stability_failure_rate_and_average_drive_preference_order() {
        let config = ScanConfig::default();
        let mut slower_stable = measured("192.0.2.1", "HK", 40.0, 30.0);
        slower_stable.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(150.0); 100],
        ));
        let mut faster_unstable = measured("192.0.2.2", "HK", 100.0, 100.0);
        let mut probes = vec![Some(20.0); 100];
        probes[0] = None;
        faster_unstable.tcp_stability = Some(crate::stability::summarize(100, 200.0, probes));
        let mut results = vec![faster_unstable, slower_stable];
        rank_results(&mut results, &config, true);
        assert_eq!(results[1].role.as_deref(), Some("primary"));
        assert_eq!(crate::model::tcp_display_ms(&results[1]), 150.0);
        assert_eq!(
            results[1].tcp_ms, 20.0,
            "initial TCP observation must stay intact"
        );
        let download = ScanConfig {
            preference: "download".into(),
            ..Default::default()
        };
        rank_results(&mut results, &download, true);
        assert_eq!(results[0].role.as_deref(), Some("primary"));
        results[1].download_mbps = 100.0;
        rank_results(&mut results, &download, true);
        assert_eq!(results[1].role.as_deref(), Some("primary"));
    }

    #[test]
    fn cancellation_drains_queued_probe_progress_without_inventing_unattempted_failures() {
        let mut row = measured("192.0.2.1", "HK", 40.0, 30.0);
        row.tcp_stability = None;
        row.download_samples_mbps.clear();
        let mut state = State {
            progress: ScanProgress {
                stage: "stabilizing".into(),
                total: 100,
                stability_batch: Some(Arc::new(StabilityBatch {
                    number: 2,
                    candidate_ips: vec![row.ip.clone()],
                })),
                ..Default::default()
            },
            results: vec![row],
            last_event: Instant::now(),
        };
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        sender
            .send((
                0,
                crate::stability::summarize(100, 200.0, vec![Some(20.0); 3]),
            ))
            .unwrap();
        sender
            .send((0, crate::stability::summarize(100, 200.0, vec![Some(20.0)])))
            .unwrap();
        drain_stability_updates(&mut state, &mut receiver);
        assert!(state.progress.message.contains("第 2 批"));
        assert!(!state.progress.message.contains("第 0 批"));
        assert!(state.progress.message.contains("耗时高于目标样本"));
        let callback: UpdateCallback = Arc::new(|_| {});
        let outcome = state.cancel(&ScanConfig::default(), ScanScope::Regional, &callback);
        let stats = outcome.results[0].tcp_stability.as_ref().unwrap();
        assert_eq!(stats.attempted, 3);
        assert_eq!(stats.succeeded, 3);
        assert_eq!(stats.failed, 0);
        assert_eq!(outcome.progress.completed, 3);
        assert!(!stats.completed && !outcome.results[0].eligible);
        assert!(outcome.progress.stability_batch.is_none());
    }

    #[test]
    fn early_rejected_snapshot_preserves_real_progress_and_cannot_be_promoted() {
        let config = ScanConfig::default();
        let mut row = measured("192.0.2.1", "HK", 40.0, 30.0);
        row.tcp_stability = None;
        let mut state = State {
            progress: ScanProgress {
                stage: "stabilizing".into(),
                total: 100,
                stability_batch: Some(Arc::new(StabilityBatch {
                    number: 1,
                    candidate_ips: vec![row.ip.clone()],
                })),
                ..Default::default()
            },
            results: vec![row],
            last_event: Instant::now(),
        };
        let mut rejected = crate::stability::summarize(100, 230.0, vec![None, None]);
        rejected.early_rejection =
            crate::stability::impossible_to_pass(&rejected.probes_ms, 100, 230.0, 1.0, "p95");
        assert!(rejected.early_rejection.is_some());
        apply_stability_snapshot(&mut state, 0, rejected);
        // An earlier unflagged sample at the same count can still be queued.
        apply_stability_snapshot(
            &mut state,
            0,
            crate::stability::summarize(100, 230.0, vec![None, None]),
        );
        let stats = state.results[0].tcp_stability.as_ref().unwrap();
        assert_eq!(state.progress.completed, 2);
        assert_eq!(state.progress.total, 100);
        assert!(state.progress.message.contains("提前淘汰"));
        assert_eq!(stats.attempted, 2);
        assert!(!stats.completed);
        assert!(stats.early_rejection.is_some());
        rank_results(&mut state.results, &config, true);
        assert!(!state.results[0].measurement_passed);
        assert!(!state.results[0].eligible);
        assert_ne!(state.results[0].role.as_deref(), Some("primary"));
        assert!(state.results[0]
            .exclusion_reason
            .as_deref()
            .unwrap()
            .contains("提前淘汰"));
    }

    #[test]
    fn precision_batch_persists_across_its_stages_and_clears_outside_precision() {
        let mut state = State {
            progress: ScanProgress {
                precision_batch: Some(Arc::new(StabilityBatch {
                    number: 3,
                    candidate_ips: vec!["192.0.2.1".into()],
                })),
                ..Default::default()
            },
            results: vec![IpResult::new(Ipv4Addr::new(192, 0, 2, 1))],
            last_event: Instant::now(),
        };
        let callback: UpdateCallback = Arc::new(|_| {});
        for stage in ["verifying", "stabilizing", "downloading"] {
            state.stage(stage, 1, String::new(), &callback);
            assert_eq!(state.progress.precision_batch.as_ref().unwrap().number, 3);
        }
        state.stage("completed", 1, String::new(), &callback);
        assert!(state.progress.precision_batch.is_none());
    }

    #[test]
    fn stability_batch_metadata_is_emitted_before_zero_progress_and_cleared_between_batches() {
        let previous = measured("192.0.2.1", "HK", 40.0, 30.0);
        let second = IpResult::new(Ipv4Addr::new(192, 0, 2, 2));
        let third = IpResult::new(Ipv4Addr::new(192, 0, 2, 3));
        let mut state = State {
            progress: Default::default(),
            results: vec![previous, second, third],
            last_event: Instant::now(),
        };
        let emitted = Arc::new(std::sync::Mutex::new(Vec::<EngineUpdate>::new()));
        let captured = emitted.clone();
        let callback: UpdateCallback = Arc::new(move |update| {
            captured.lock().unwrap().push(update);
        });
        state.begin_stability_batch(2, &[1], 100, &callback);
        let mut progress_updates = ProgressUpdates::new(&state, &callback);
        {
            let emitted = emitted.lock().unwrap();
            let first = emitted.last().unwrap();
            assert_eq!(first.progress.completed, 0);
            assert_eq!(first.progress.total, 100);
            let batch = first.progress.stability_batch.as_ref().unwrap();
            assert_eq!(batch.number, 2);
            assert_eq!(batch.candidate_ips, ["192.0.2.2"]);
            assert_eq!(
                first.results.as_ref().unwrap()[0]
                    .tcp_stability
                    .as_ref()
                    .unwrap()
                    .attempted,
                100
            );
        }
        apply_stability_snapshot(
            &mut state,
            1,
            crate::stability::summarize(100, 230.0, vec![]),
        );
        assert_eq!(state.progress.completed, 0);
        assert_eq!(
            state.results[1].tcp_stability.as_ref().unwrap().attempted,
            0
        );
        assert!(state.progress.message.contains("第 2 批"));
        apply_stability_snapshot(
            &mut state,
            0,
            crate::stability::summarize(100, 230.0, vec![Some(10.0); 100]),
        );
        assert_eq!(
            state.progress.completed, 0,
            "previous batch must not enter this batch's counters"
        );
        assert!(apply_stability_snapshot(
            &mut state,
            1,
            crate::stability::summarize(100, 230.0, vec![Some(10.0); 5]),
        ));
        assert_eq!(state.progress.completed, 5);
        progress_updates.changed.insert(1);
        progress_updates.emit(&mut state, true);
        {
            let emitted = emitted.lock().unwrap();
            let update = emitted.last().unwrap();
            assert!(update.results.is_none());
            assert_eq!(update.progress.completed, 5);
            assert_eq!(update.changed_rows.keys().copied().collect::<Vec<_>>(), [1]);
        }
        state.stage("downloading", 2, "下载复测".into(), &callback);
        assert!(state.progress.stability_batch.is_none());
        assert!(emitted
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .progress
            .stability_batch
            .is_none());
        state.begin_stability_batch(3, &[2], 100, &callback);
        assert_eq!(state.progress.completed, 0);
        let batch = state.progress.stability_batch.as_ref().unwrap();
        assert_eq!(batch.number, 3);
        assert_eq!(batch.candidate_ips, ["192.0.2.3"]);
        apply_stability_snapshot(
            &mut state,
            1,
            crate::stability::summarize(100, 230.0, vec![Some(10.0); 6]),
        );
        assert_eq!(state.progress.completed, 0);
        apply_stability_snapshot(
            &mut state,
            2,
            crate::stability::summarize(100, 230.0, vec![]),
        );
        assert_eq!(state.progress.completed, 0);
        assert!(state.progress.message.contains("第 3 批"));
    }

    #[test]
    fn p95_gate_uses_actual_samples_and_keeps_high_peaks_as_observations() {
        let config = ScanConfig {
            max_tcp_ms: Some(200.0),
            ..Default::default()
        };
        let mut row = measured("192.0.2.1", "HK", 40.0, 30.0);
        let mut probes = vec![Some(200.0); 95];
        probes.extend(vec![Some(2000.0); 5]);
        row.tcp_stability = Some(crate::stability::summarize(100, 200.0, probes));
        assert!(row.stability_exclusion(&config).is_none());
        assert_eq!(crate::model::tcp_display_ms(&row), 200.0);
        assert_eq!(row.tcp_stability.as_ref().unwrap().max_ms, Some(2000.0));
        let average = ScanConfig {
            stability_latency_metric: "average".into(),
            max_tcp_ms: Some(200.0),
            ..Default::default()
        };
        assert!(row.stability_exclusion(&average).is_some());
        let mut probes = vec![Some(200.0); 94];
        probes.extend(vec![Some(200.001); 6]);
        row.tcp_stability = Some(crate::stability::summarize(100, 200.0, probes));
        row.tcp_stability.as_mut().unwrap().p95_ms = Some(0.0);
        assert!(row
            .stability_exclusion(&config)
            .unwrap()
            .contains("200.001"));
        assert_eq!(crate::model::tcp_display_ms(&row), 200.001);
        row.tcp_stability = Some(crate::stability::summarize(
            100,
            200.0,
            vec![Some(150.0); 100],
        ));
        row.tcp_stability.as_mut().unwrap().p95_ms = Some(300.0);
        assert!(row
            .stability_exclusion(&config)
            .unwrap()
            .contains("原始样本不一致"));
        assert_eq!(crate::model::tcp_display_ms(&row), 150.0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn isolated_tcp_timing_does_not_include_a_blocked_parent_progress_consumer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                drop(stream);
            }
        });
        let work =
            stream::iter((0..2).map(|_| isolated_tcp_probe(address, Duration::from_secs(1))))
                .buffer_unordered(2);
        tokio::pin!(work);
        let first = work.next().await.unwrap().unwrap();
        let blocked_at = Instant::now();
        std::thread::sleep(Duration::from_millis(100));
        let second = work.next().await.unwrap().unwrap();
        assert!(blocked_at.elapsed() >= Duration::from_millis(100));
        assert!(
            first < 90.0 && second < 90.0,
            "parent callback inflated loopback timings: {first}, {second}"
        );
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stability_worker_returns_after_two_failures_and_releases_its_slot() {
        // Keep a bound, non-listening socket alive so connections cannot
        // succeed and parallel tests cannot reuse it for a reachable listener.
        let reserved = tokio::net::TcpSocket::new_v4().unwrap();
        reserved.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = reserved.local_addr().unwrap();
        let config = ScanConfig {
            stability_interval_ms: 1,
            connect_timeout_ms: 100,
            ..Default::default()
        };
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut workers = tokio::task::JoinSet::new();
        spawn_stability_worker(
            &mut workers,
            0,
            address,
            &config,
            CancellationToken::new(),
            sender,
        );
        let (index, stats) = tokio::time::timeout(Duration::from_secs(2), workers.join_next())
            .await
            .expect("known rejected worker did not release its slot")
            .unwrap()
            .unwrap();
        assert_eq!(index, 0);
        assert!(workers.is_empty());
        assert_eq!(stats.attempted, 2);
        assert_eq!(stats.failed, 2);
        assert!(!stats.completed);
        assert!(stats.early_rejection.is_some());
        let mut last = None;
        while let Ok((_, snapshot)) = receiver.try_recv() {
            last = Some(snapshot);
        }
        let last = last.expect("terminal early rejection was not emitted");
        assert_eq!(last.attempted, stats.attempted);
        assert!(last.early_rejection.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn isolated_stability_worker_continues_during_a_blocked_parent_and_aborts_on_drop() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = accepted.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                drop(stream);
            }
        });
        let config = ScanConfig {
            stability_attempts: Some(1000),
            stability_interval_ms: 1,
            ..Default::default()
        };
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut workers = tokio::task::JoinSet::new();
        spawn_stability_worker(
            &mut workers,
            0,
            address,
            &config,
            CancellationToken::new(),
            sender,
        );
        while receiver.recv().await.unwrap().1.attempted == 0 {}
        std::thread::sleep(Duration::from_millis(100));
        let mut latest = None;
        while let Ok((_, stats)) = receiver.try_recv() {
            latest = Some(stats);
        }
        let latest = latest.expect("independent worker stopped with its parent");
        assert!(latest.attempted > 1);
        assert!(
            latest.max_ms.unwrap() < 90.0,
            "blocked parent polluted TCP probe timing"
        );
        drop(workers); // JoinSet drop must abort every owned worker.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let before = accepted.load(std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::Relaxed),
            before,
            "dropped worker continued opening connections"
        );
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn download_schedule_rotates_regions_and_completes_first_choices_first() {
        let groups = BTreeMap::from([
            ("HK".into(), vec![0, 1]),
            ("JP".into(), vec![2]),
            ("US".into(), vec![3, 4]),
        ]);
        let regions = vec!["HK".into(), "JP".into(), "US".into()];
        let expected = vec![0, 2, 3, 0, 2, 3, 1, 4, 1, 4];
        assert_eq!(download_schedule(&groups, &regions, 2), expected);
        let mut queue = DownloadQueue::new(&groups, &regions, 2);
        let actual: Vec<_> = std::iter::from_fn(|| queue.next(None, 0))
            .map(|slot| match slot {
                DownloadSlot::Round {
                    index,
                    allowance: None,
                } => index,
                _ => panic!("unlimited downloads should never be budgeted"),
            })
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn cloudflare_downloads_are_bounded_and_custom_queries_are_preserved() {
        let target =
            Url::parse("https://speed.cloudflare.com/__down?bytes=25000000&foo=bar").unwrap();
        let (url, bytes) = bounded_download_url(&target, 1048576);
        assert_eq!(bytes, 1048576);
        assert!(url
            .query_pairs()
            .any(|(key, val)| key == "bytes" && val == "1048576"));
        assert!(url
            .query_pairs()
            .any(|(key, val)| key == "foo" && val == "bar"));
        let custom = Url::parse("https://test.example/file?bytes=99&token=a%2Bb").unwrap();
        let (url, bytes) = bounded_download_url(&custom, 2048);
        assert_eq!(url, custom);
        assert_eq!(bytes, 2048);
    }

    #[test]
    fn median_range_and_ray_parsing_are_finite_and_unambiguous() {
        assert_eq!(median(&[]), 0.0);
        assert_eq!(median(&[30.0, 10.0, 20.0]), 20.0);
        assert_eq!(median(&[30.0, 10.0]), 20.0);
        assert_eq!(spread(&[30.0, 10.0, 20.0]), 20.0);
        let mut headers = header::HeaderMap::new();
        headers.insert("cf-ray", "abcdef-NRT".parse().unwrap());
        assert_eq!(ray_colo(&headers).as_deref(), Some("NRT"));
        headers.append("cf-ray", "123abc-HKG".parse().unwrap());
        assert_eq!(ray_colo(&headers), None);
        for value in ["HKG", "-HKG", "not-a-POP", "abcdef-HKG, 123abc-NRT"] {
            headers.insert("cf-ray", value.parse().unwrap());
            assert_eq!(ray_colo(&headers), None);
        }
    }

    #[test]
    fn download_checks_all_content_and_transfer_encoding_values() {
        let mut headers = header::HeaderMap::new();
        assert!(has_unencoded_body(&headers));
        headers.insert(header::CONTENT_ENCODING, "Identity".parse().unwrap());
        assert!(has_unencoded_body(&headers));
        headers.append(header::CONTENT_ENCODING, "gzip".parse().unwrap());
        assert!(!has_unencoded_body(&headers));
        headers.insert(header::CONTENT_ENCODING, "identity, br".parse().unwrap());
        assert!(!has_unencoded_body(&headers));
        headers.remove(header::CONTENT_ENCODING);
        headers.insert(header::TRANSFER_ENCODING, "Chunked".parse().unwrap());
        assert!(has_unencoded_body(&headers));
        headers.insert(header::TRANSFER_ENCODING, "gzip, chunked".parse().unwrap());
        assert!(!has_unencoded_body(&headers));
        headers.insert(header::TRANSFER_ENCODING, "gzip".parse().unwrap());
        headers.append(header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        assert!(!has_unencoded_body(&headers));
    }

    #[tokio::test]
    async fn tcp_measures_a_real_local_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ms = measure_tcp_connect(listener.local_addr().unwrap(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!((0.0..1000.0).contains(&ms));
    }

    #[tokio::test]
    async fn initial_tcp_failure_retains_diagnostics() {
        let reserved = tokio::net::TcpSocket::new_v4().unwrap();
        reserved.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = reserved.local_addr().unwrap().port();
        let config = ScanConfig {
            target_url: format!("https://example.com:{port}/"),
            ..custom("127.0.0.1", 1)
        };
        let outcome = run_scan(config, CancellationToken::new(), Arc::new(|_| {}))
            .await
            .unwrap();
        let result = &outcome.results[0];
        let failure = result.tcp_connect_failure.as_ref().unwrap();
        assert_eq!(result.tcp_ms, 0.0);
        assert_eq!(result.exclusion_code, Some(ExclusionCode::TcpConnect));
        assert_eq!(
            result.exclusion_reason.as_deref(),
            Some(failure.exclusion_reason().as_str())
        );
        // A non-listening port can reach the probe deadline before the OS
        // reports refusal. Both outcomes must reach the initial result.
        assert!(matches!(
            failure.kind,
            TcpConnectFailureKind::Connect | TcpConnectFailureKind::Timeout
        ));
        assert!(failure
            .elapsed_ms
            .is_some_and(|ms| ms.is_finite() && ms >= 0.0));
        let snapshot = serde_json::to_value(result).unwrap();
        assert!(snapshot["tcpConnectFailure"]["elapsedMs"].is_number());
        let restored: IpResult = serde_json::from_value(snapshot).unwrap();
        let restored = restored.tcp_connect_failure.unwrap();
        assert_eq!(restored.kind, failure.kind);
        assert_eq!(restored.message, failure.message);
    }

    #[tokio::test]
    async fn cancellation_preserves_partial_results_without_recommendations() {
        let token = CancellationToken::new();
        token.cancel();
        let config = custom("192.0.2.1", 1);
        let outcome = run_scan(config, token, Arc::new(|_| {})).await.unwrap();
        assert!(outcome.cancelled);
        assert_eq!(outcome.progress.stage, "cancelled");
        assert_eq!(outcome.results.len(), 1);
        assert!(!outcome.results[0].eligible);
        assert_eq!(outcome.results[0].role.as_deref(), Some("candidate"));
    }

    #[tokio::test]
    async fn download_only_starts_with_a_tls_request_and_skips_stability_and_verification() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.read_u8().await.unwrap()
        });
        let config = ScanConfig {
            target_url: format!("https://ip-picker-fixture.invalid:{port}/download"),
            regions: Vec::new(),
            stability_attempts: None,
            verification_attempts: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        assert!(ScanConfig {
            download_rounds: 1,
            ..config.clone()
        }
        .validate_download_only()
        .is_err());
        let stages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = stages.clone();
        let outcome = run_download_test(
            config.clone(),
            vec![Ipv4Addr::LOCALHOST],
            CancellationToken::new(),
            Arc::new(move |update| captured.lock().unwrap().push(update.progress.stage)),
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap(),
            0x16
        );
        let stages = stages.lock().unwrap();
        assert_eq!(stages.first().unwrap(), "downloading");
        assert_eq!(stages.last().unwrap(), "completed");
        assert!(stages
            .iter()
            .all(|stage| stage == "downloading" || stage == "completed"));
        assert_eq!(outcome.progress.completed, config.download_rounds);
        let row = &outcome.results[0];
        assert_eq!(row.exclusion_code, Some(ExclusionCode::HttpsFailure));
        assert_eq!(row.attempts, 0);
        assert!(row.tcp_stability.is_none());
        assert!(!row.measurement_passed);
        assert!(!row.eligible);
    }

    #[tokio::test]
    async fn invalid_configuration_opens_no_connection_and_emits_no_result() {
        let config = ScanConfig {
            target_url: "http://localhost/".into(),
            ..Default::default()
        };
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = count.clone();
        assert!(run_scan(
            config,
            CancellationToken::new(),
            Arc::new(move |_| {
                captured.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
        )
        .await
        .is_err());
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio_rustls::{
        rustls::{self, pki_types::PrivatePkcs8KeyDer},
        TlsAcceptor,
    };

    const FIXTURE_HOST: &str = "ip-picker-fixture.invalid";

    #[derive(Debug)]
    struct ObservedRequest {
        sni: Option<String>,
        request: String,
    }

    struct TlsFixture {
        address: SocketAddr,
        certificate: Vec<u8>,
        observation: tokio::sync::oneshot::Receiver<Result<ObservedRequest, String>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl TlsFixture {
        fn target(&self) -> Url {
            Url::parse(&format!(
                "https://{FIXTURE_HOST}:{}/cdn-cgi/trace",
                self.address.port()
            ))
            .unwrap()
        }

        /// Trust exists only in the test binary. Production always builds its
        /// client from the normal platform-independent rustls trust roots.
        fn trusted_client(&self) -> Client {
            client_builder_for_ip(&ScanConfig::default(), &self.target(), Ipv4Addr::LOCALHOST)
                .unwrap()
                .timeout(Duration::from_secs(3))
                .add_root_certificate(reqwest::Certificate::from_der(&self.certificate).unwrap())
                .build()
                .unwrap()
        }

        async fn finish(self) -> Result<ObservedRequest, String> {
            let observed = tokio::time::timeout(Duration::from_secs(5), self.observation)
                .await
                .expect("fixture observation timed out")
                .expect("fixture observation sender disappeared");
            self.task.await.expect("fixture task panicked");
            observed
        }
    }

    async fn fixture(certificate_host: &str, redirect: bool) -> TlsFixture {
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec![certificate_host.to_string()]).unwrap();
        let certificate = cert.der().to_vec();
        let key = PrivatePkcs8KeyDer::from(key_pair.serialize_der());
        // Set the provider locally rather than changing global TLS state while
        // independent tests run in parallel.
        let mut server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key.into())
        .unwrap();
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, observation) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let outcome = tokio::time::timeout(Duration::from_secs(4), async {
                let (socket, _) = listener.accept().await.map_err(|e| e.to_string())?;
                let mut tls = acceptor.accept(socket).await.map_err(|e| e.to_string())?;
                let sni = tls.get_ref().1.server_name().map(str::to_owned);
                let request = read_http_fixture_headers(&mut tls).await.map_err(|e| e.to_string())?;
                let request = String::from_utf8(request).map_err(|e| e.to_string())?;
                let response = if redirect {
                    "HTTP/1.1 302 Found\r\nLocation: https://redirect-target.invalid/other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    let body = format!("h={FIXTURE_HOST}\ncolo=NRT\nloc=CN\n");
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nCF-Ray: abcdef-NRT\r\nConnection: close\r\n\r\n{body}", body.len())
                };
                tls.write_all(response.as_bytes()).await.map_err(|e| e.to_string())?;
                tls.shutdown().await.map_err(|e| e.to_string())?;
                Ok(ObservedRequest { sni, request })
            })
            .await
            .unwrap_or_else(|_| Err("fixture connection timed out".into()));
            let _ = sender.send(outcome);
        });
        TlsFixture {
            address,
            certificate,
            observation,
            task,
        }
    }

    #[tokio::test]
    async fn pinned_https_preserves_target_sni_host_and_checks_the_real_peer() {
        let fixture = fixture(FIXTURE_HOST, false).await;
        let target = fixture.target();
        let expected_host = format!("{FIXTURE_HOST}:{}", fixture.address.port());
        // .invalid cannot resolve publicly: success proves the request reached
        // our pinned loopback candidate while TLS still used the target name.
        let response = fixture
            .trusted_client()
            .get(target.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.remote_addr(), Some(fixture.address));
        validate_response(&response, Ipv4Addr::LOCALHOST).unwrap();
        let wrong_peer = validate_response(&response, Ipv4Addr::new(192, 0, 2, 1)).unwrap_err();
        assert_eq!(wrong_peer.code, ExclusionCode::PopDrift);
        assert!(wrong_peer.message.contains("连接对端发生变化"));
        let body = response.text().await.unwrap();
        assert_eq!(parse_trace(&body, &target).unwrap(), "NRT");
        let observed = fixture.finish().await.unwrap();
        assert_eq!(observed.sni.as_deref(), Some(FIXTURE_HOST));
        assert!(observed
            .request
            .starts_with("GET /cdn-cgi/trace HTTP/1.1\r\n"));
        let host = observed
            .request
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.trim());
        assert_eq!(host, Some(expected_host.as_str()));
    }

    #[tokio::test]
    async fn production_https_rejects_an_untrusted_self_signed_certificate() {
        let fixture = fixture(FIXTURE_HOST, false).await;
        let target = fixture.target();
        // This is exactly the production client, with no test root installed.
        let client = client_for_ip(&ScanConfig::default(), &target, Ipv4Addr::LOCALHOST).unwrap();
        let error = client
            .get(target)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .expect_err("accepted self-signed certificate");
        assert!(request_error(error).message.contains("TLS 证书验证失败"));
        assert!(fixture.finish().await.is_err());
    }

    #[tokio::test]
    async fn trusted_root_does_not_disable_target_hostname_verification() {
        let fixture = fixture("different-fixture.invalid", false).await;
        let error = fixture
            .trusted_client()
            .get(fixture.target())
            .send()
            .await
            .expect_err("accepted a certificate for a different hostname");
        assert!(request_error(error).message.contains("TLS 证书验证失败"));
        assert!(fixture.finish().await.is_err());
    }

    #[tokio::test]
    async fn pinned_https_does_not_follow_redirects_to_another_host_or_ip() {
        let fixture = fixture(FIXTURE_HOST, true).await;
        let target = fixture.target();
        let response = fixture
            .trusted_client()
            .get(target.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.url(), &target);
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        let reason = validate_response(&response, Ipv4Addr::LOCALHOST).unwrap_err();
        assert!(reason.message.contains("重定向已拒绝"));
        // If redirects were enabled .send() would have tried the unresolvable
        // redirect target and failed instead of returning this original 302.
        assert_eq!(
            fixture.finish().await.unwrap().sni.as_deref(),
            Some(FIXTURE_HOST)
        );
    }
}

//! Real, bounded measurements. Every HTTPS request uses its own HTTP/1 client,
//! binds the URL host to exactly one candidate, validates TLS normally, and
//! checks the connected peer. Results describe the current network and target.

use crate::model::{
    compare_ip_results_with_metric, pop, IpResult, ScanConfig, ScanProgress, StabilityBatch,
};
use futures_util::{stream, StreamExt};
use ipnet::Ipv4Net;
use rand::Rng;
use reqwest::{header, redirect::Policy, Client, Response};
use std::{
    collections::{BTreeMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::net::TcpStream;
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
    pub results: Vec<IpResult>,
}

#[derive(Debug, Clone)]
pub struct EngineOutcome {
    pub progress: ScanProgress,
    pub results: Vec<IpResult>,
    pub cancelled: bool,
}

pub type UpdateCallback = Arc<dyn Fn(EngineUpdate) + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanScope {
    Regional,
    Targeted,
}

struct State {
    progress: ScanProgress,
    results: Vec<IpResult>,
    last_event: Instant,
}

impl State {
    fn emit(&mut self, callback: &UpdateCallback, force: bool) {
        if force || self.last_event.elapsed() >= EVENT_INTERVAL {
            callback(EngineUpdate {
                progress: self.progress.clone(),
                results: self.results.clone(),
            });
            self.last_event = Instant::now();
        }
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
        self.progress.stability_batch = Some(StabilityBatch {
            number,
            candidate_ips: candidates
                .iter()
                .map(|index| self.results[*index].ip.clone())
                .collect(),
        });
        self.stage(
            "stabilizing",
            candidates.len() * attempts,
            format!("第 {number} 批 TCP 稳定性：每个候选 {attempts} 次独立连接"),
            callback,
        );
    }

    fn cancel(mut self, config: &ScanConfig, callback: &UpdateCallback) -> EngineOutcome {
        for item in &mut self.results {
            if item.exclusion_reason.is_none()
                && item.download_samples_mbps.len() < config.download_rounds
            {
                item.exclusion_reason = Some("测试已停止，验证或下载复测未完成".into());
            }
        }
        rank_results(&mut self.results, config, false);
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
    config.validate()?;
    let ips = sample_candidates(&config)?;
    run_candidates(config, ips, ScanScope::Regional, cancellation, callback).await
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
            network_path: crate::network::capture(ips[0]),
            ..Default::default()
        },
        results: ips.iter().map(|ip| IpResult::new(*ip)).collect(),
        last_event: Instant::now(),
    };
    state.stage(
        "connecting",
        ips.len(),
        if scope == ScanScope::Targeted {
            format!("TCP 连接检查：{} 个指定收藏 IPv4", ips.len())
        } else {
            format!("TCP 连接筛选：{} 个抽样 IPv4", ips.len())
        },
        &callback,
    );
    let tcp = stream::iter(ips.iter().copied().enumerate().map(|(index, ip)| {
        let deadline = Duration::from_millis(config.connect_timeout_ms);
        async move {
            (
                index,
                isolated_tcp_probe(SocketAddr::new(IpAddr::V4(ip), port), deadline).await,
            )
        }
    }))
    .buffer_unordered(config.tcp_concurrency);
    tokio::pin!(tcp);
    while let Some((index, outcome)) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => { return Ok(state.cancel(&config, &callback)); }
        next = tcp.next() => next
    } {
        match outcome {
            Ok(ms) => apply_tcp_measurement(&mut state.results[index], ms, &config),
            Err(reason) => state.results[index].exclusion_reason = Some(reason),
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
        _ = cancellation.cancelled() => { return Ok(state.cancel(&config, &callback)); }
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
                    item.exclusion_reason = Some(format!(
                        "POP {} 尚无地区映射，保留观测但不推测归属",
                        item.colo
                    ));
                } else if scope == ScanScope::Regional
                    && !config.regions.contains(&item.region_code)
                {
                    item.exclusion_reason = Some("实际响应地区不在本次选择范围".into());
                }
            }
            Err(reason) => item.exclusion_reason = Some(reason),
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
        if item.exclusion_reason.is_none() && !selected.contains(&index) {
            item.exclusion_reason = Some(format!(
                "按初筛 TCP 延迟排序，未进入本地区前 {} 名精测候选（未精测）",
                regional_precision_limit(&config)
            ));
        }
    }
    // Finish the whole bounded regional pool before choosing any downloads.
    // Only two cumulative connection failures can stop an individual probe early.
    let stability_candidates = download_schedule(&groups, &config.regions, 1);
    state.progress.precision_batch = Some(StabilityBatch {
        number: 1,
        candidate_ips: stability_candidates
            .iter()
            .map(|index| state.results[*index].ip.clone())
            .collect(),
    });
    let attempts = config
        .stability_attempts
        .expect("validated stability count");
    state.begin_stability_batch(1, &stability_candidates, attempts, &callback);
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
                return Ok(state.cancel(&config, &callback));
            }
            Some((index, snapshot)) = stability_updates.recv() => {
                apply_stability_snapshot(&mut state, index, snapshot);
                state.emit(&callback, false);
            }
            result = stability_workers.join_next() => {
                if let Some(Ok((index, snapshot))) = result {
                    apply_stability_snapshot(&mut state, index, snapshot);
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
        return Ok(state.cancel(&config, &callback));
    }
    for index in &stability_candidates {
        if let Some(reason) = state.results[*index].stability_exclusion(&config) {
            state.results[*index].exclusion_reason = Some(reason);
        }
    }

    let download_groups = choose_download_candidates(&mut state.results, &groups, &config, scope);
    let download_candidates = download_schedule(&download_groups, &config.regions, 1);
    state.progress.precision_batch = Some(StabilityBatch {
        number: 1,
        candidate_ips: download_candidates
            .iter()
            .map(|index| state.results[*index].ip.clone())
            .collect(),
    });
    state.stage(
        "verifying",
        download_candidates.len(),
        if scope == ScanScope::Targeted {
            format!(
                "TCP 稳定性已全部结束，验证全部通过稳定性检查的收藏 IP：每个 IP 共验证 {} 次 HTTPS",
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
        return Ok(state.cancel(&config, &callback));
    }

    let verified_groups = download_groups
        .into_iter()
        .map(|(region, entries)| {
            let entries = entries
                .into_iter()
                .filter(|index| state.results[*index].exclusion_reason.is_none())
                .collect();
            (region, entries)
        })
        .collect();
    let schedule = download_schedule(&verified_groups, &config.regions, config.download_rounds);
    let total_budget = config.download_budget_bytes();
    let mut used = 0_u64;
    state.stage(
        "downloading",
        schedule.len(),
        if scope == ScanScope::Targeted {
            if config.download_budget_enabled {
                "对全部验证通过的收藏 IP 进行下载复测，共享本轮下载预算".into()
            } else {
                "对全部验证通过的收藏 IP 进行下载复测，不限制下载流量".into()
            }
        } else {
            format!("各区仅对稳定性择优的前 {DOWNLOAD_CANDIDATES_PER_REGION} 个进行下载复测")
        },
        &callback,
    );
    for (slot, index) in schedule.iter().copied().enumerate() {
        if cancellation.is_cancelled() {
            state.progress.downloaded_bytes = used;
            return Ok(state.cancel(&config, &callback));
        }
        if state.results[index].exclusion_reason.is_some() {
            state.progress.completed += 1;
            continue;
        }
        let slots_left = schedule[slot..]
            .iter()
            .filter(|i| state.results[**i].exclusion_reason.is_none())
            .count() as u64;
        let allowance = total_budget.map(|total| download_allowance(total, used, slots_left));
        if allowance.is_some_and(|bytes| bytes < MIN_SAMPLE_BYTES) {
            state.results[index].exclusion_reason = Some("下载预算不足，未完成必要复测".into());
            state.progress.completed += 1;
            state.emit(&callback, false);
            continue;
        }
        let label = if state.results[index].city.is_empty() {
            state.results[index].colo.clone()
        } else {
            state.results[index].city.clone()
        };
        state.progress.message = format!(
            "下载复测 {label} · {} · 第 {} / {} 轮",
            state.results[index].ip,
            state.results[index].download_samples_mbps.len() + 1,
            config.download_rounds
        );
        state.emit(&callback, true);

        // Poll the byte counter while streaming so progress and cancellation stay
        // responsive even when a server delivers a slow or endless response.
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter_for_probe = counter.clone();
        let download = download_probe(&config, &target, ips[index], allowance, counter_for_probe);
        tokio::pin!(download);
        let mut ticker = tokio::time::interval(EVENT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let measured = loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    state.progress.downloaded_bytes = used + counter.load(std::sync::atomic::Ordering::Relaxed);
                    return Ok(state.cancel(&config, &callback));
                }
                result = &mut download => break result,
                _ = ticker.tick() => {
                    state.progress.downloaded_bytes = used + counter.load(std::sync::atomic::Ordering::Relaxed);
                    state.emit(&callback, false);
                }
            }
        };
        used += counter.load(std::sync::atomic::Ordering::Relaxed);
        state.progress.downloaded_bytes = used;
        match measured {
            Ok(sample) => {
                if !record_download(
                    &mut state.results[index],
                    sample,
                    &config,
                    &cancellation,
                    || trace_probe(&config, &target, ips[index]),
                )
                .await
                {
                    return Ok(state.cancel(&config, &callback));
                }
            }
            Err(reason) => state.results[index].exclusion_reason = Some(reason),
        }
        state.progress.completed += 1;
        state.emit(&callback, false);
    }

    if cancellation.is_cancelled() {
        return Ok(state.cancel(&config, &callback));
    }
    for item in &mut state.results {
        if item.exclusion_reason.is_none()
            && item.download_samples_mbps.len() < config.download_rounds
        {
            item.exclusion_reason = Some("未完成必要的下载复测轮次".into());
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
            "收藏 IP 测试完成，{passed} / {} 个通过完整测量",
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

fn apply_stability_snapshot(
    state: &mut State,
    index: usize,
    snapshot: crate::stability::TcpStability,
) {
    if state.progress.stage != "stabilizing" {
        return;
    }
    if state
        .progress
        .stability_batch
        .as_ref()
        .is_some_and(|batch| !batch.candidate_ips.contains(&state.results[index].ip))
    {
        return;
    }
    let previous = state.results[index].tcp_stability.as_ref();
    let previous_attempted = previous.map_or(0, |stats| stats.attempted);
    if snapshot.attempted < previous_attempted
        || previous.is_some_and(|stats| {
            (stats.completed && !snapshot.completed)
                || (stats.early_rejection.is_some() && snapshot.early_rejection.is_none())
        })
    {
        return;
    }
    state.progress.completed += snapshot.attempted - previous_attempted;
    let prefix = state
        .progress
        .stability_batch
        .as_ref()
        .map(|batch| format!("第 {} 批 · ", batch.number))
        .unwrap_or_default();
    state.progress.message = if let Some(reason) = &snapshot.early_rejection {
        state.results[index].exclusion_reason = Some(reason.clone());
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

async fn isolated_tcp_probe(addr: SocketAddr, timeout: Duration) -> Result<f64, String> {
    // Record completion on an independent worker; a slow progress consumer must
    // not delay polling an already-ready socket and inflate its elapsed time.
    let worker = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(tcp_probe(addr, timeout)));
    worker
        .await
        .map_err(|error| format!("TCP 探测任务失败：{error}"))?
}

async fn tcp_probe(addr: SocketAddr, timeout: Duration) -> Result<f64, String> {
    let started = Instant::now();
    match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => {
            drop(stream);
            Ok(started.elapsed().as_secs_f64() * 1000.0)
        }
        Ok(Err(e)) => Err(format!("TCP 连接失败：{e}")),
        Err(_) => Err("TCP 连接超时".into()),
    }
}

fn apply_tcp_measurement(result: &mut IpResult, milliseconds: f64, config: &ScanConfig) {
    result.tcp_ms = milliseconds;
    if let Some(limit) = config.max_tcp_ms.filter(|_| config.tcp_latency_screening) {
        if milliseconds > limit {
            result.measurement_passed = false;
            result.eligible = false;
            result.role = Some("candidate".into());
            result.exclusion_reason =
                Some(format!("TCP 延迟 {milliseconds} ms，超过上限 {limit} ms"));
        }
    }
}

fn pop_discovery_candidates(results: &[IpResult], ips: &[Ipv4Addr]) -> Vec<(usize, Ipv4Addr)> {
    results
        .iter()
        .enumerate()
        .filter(|(_, result)| result.exclusion_reason.is_none())
        .map(|(index, _)| (index, ips[index]))
        .collect()
}

fn client_for_ip(
    config: &ScanConfig,
    target: &Url,
    ip: Ipv4Addr,
    timeout: Duration,
) -> Result<Client, String> {
    client_builder_for_ip(config, target, ip, timeout)?
        .build()
        .map_err(|e| format!("无法创建 HTTPS 测速连接：{e}"))
}

fn client_builder_for_ip(
    config: &ScanConfig,
    target: &Url,
    ip: Ipv4Addr,
    timeout: Duration,
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
        .timeout(timeout)
        .resolve(host, addr)
        .user_agent(concat!("IP-Picker/", env!("CARGO_PKG_VERSION"))))
}

fn validate_response(response: &Response, ip: Ipv4Addr) -> Result<(), String> {
    match response.remote_addr() {
        Some(peer) if peer.ip() == IpAddr::V4(ip) => {}
        Some(peer) => return Err(format!("连接对端发生变化（{peer}），已拒绝使用该测量")),
        None => return Err("无法确认实际连接的候选 IP".into()),
    }
    let status = response.status();
    if status.is_redirection() {
        return Err(format!(
            "HTTP {} 重定向已拒绝，防止测速跳转至其他 IP",
            status.as_u16()
        ));
    }
    if !status.is_success() {
        return Err(format!(
            "HTTPS 响应状态不符合预期：HTTP {}",
            status.as_u16()
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
    Fut: std::future::Future<Output = Result<TraceProbe, String>>,
{
    let pending: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|index| {
            let item = &state.results[*index];
            item.attempts < config.verification_attempts && item.exclusion_reason.is_none()
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
            Err(reason) => {
                item.exclusion_reason = Some(format!("HTTPS 复测失败：{reason}"));
            }
        }
        item.https_ms = median(samples);
        item.jitter_ms = spread(samples);
        // One failed service check or cross-region POP observation excludes this
        // candidate immediately; other candidates can use its released slot.
        if item.attempts < config.verification_attempts && item.exclusion_reason.is_none() {
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
) -> (usize, Result<TraceProbe, String>)
where
    F: Fn(Ipv4Addr) -> Fut,
    Fut: std::future::Future<Output = Result<TraceProbe, String>>,
{
    (index, probe(ip).await)
}

async fn record_download<F, Fut>(
    item: &mut IpResult,
    sample: DownloadProbe,
    config: &ScanConfig,
    cancellation: &CancellationToken,
    probe: F,
) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<TraceProbe, String>>,
{
    // The download has finished even if its additional POP check is cancelled.
    item.download_samples_mbps.push(sample.mbps);
    item.download_mbps = median(&item.download_samples_mbps);
    item.measurement_passed = false;
    if let Some(colo) = sample.colo {
        observe_colo(item, &colo);
    } else {
        // This explicit incomplete reason also gates a saved final-round sample
        // when cancellation happens before the same-host POP check completes.
        item.exclusion_reason = Some("下载后的 POP 复核未完成".into());
        let check = tokio::select! {
            biased;
            _ = cancellation.cancelled() => { return false; }
            outcome = probe() => outcome
        };
        item.attempts += 1;
        match check {
            Ok(probe) => {
                item.success_count += 1;
                item.exclusion_reason = None;
                observe_colo(item, &probe.colo);
            }
            Err(reason) => {
                item.exclusion_reason = Some(format!("下载后的 POP 复核失败：{reason}"));
            }
        }
    }
    item.measurement_passed = item.has_complete_measurement(config);
    true
}

async fn trace_probe(
    config: &ScanConfig,
    target: &Url,
    ip: Ipv4Addr,
) -> Result<TraceProbe, String> {
    let mut trace_url = target.clone();
    trace_url.set_path("/cdn-cgi/trace");
    trace_url.set_query(None);
    let client = client_for_ip(
        config,
        target,
        ip,
        Duration::from_millis(config.request_timeout_ms),
    )?;
    let started = Instant::now();
    let mut response = client
        .get(trace_url)
        .header(header::CONNECTION, "close")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(request_error)?;
    let https_ms = started.elapsed().as_secs_f64() * 1000.0;
    validate_response(&response, ip)?;
    if response.content_length().unwrap_or(0) > MAX_TRACE_BYTES as u64 {
        return Err("trace 响应过大，目标可能不是 Cloudflare trace 端点".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if bytes.len() + chunk.len() > MAX_TRACE_BYTES {
            return Err("trace 响应超过 16 KiB 限制".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = std::str::from_utf8(&bytes).map_err(|_| "trace 响应不是有效文本")?;
    let colo = parse_trace(body, target)?;
    Ok(TraceProbe { https_ms, colo })
}

fn parse_trace(body: &str, target: &Url) -> Result<String, String> {
    let mut colo = None;
    let mut host = None;
    for line in body.lines() {
        if let Some(value) = line.strip_prefix("colo=") {
            if colo.is_some() {
                return Err("trace 含有重复 colo 字段".into());
            }
            colo = Some(value.trim());
        }
        if let Some(value) = line.strip_prefix("h=") {
            host = Some(value.trim());
        }
    }
    let colo = colo
        .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase()))
        .ok_or("未获得有效的 Cloudflare POP 标识（colo）")?;
    let expected = target.host_str().ok_or("测速地址缺少域名")?;
    let expected_port = target.port().map(|p| format!("{expected}:{p}"));
    if !host.is_some_and(|h| {
        h.eq_ignore_ascii_case(expected)
            || expected_port
                .as_deref()
                .is_some_and(|p| h.eq_ignore_ascii_case(p))
    }) {
        return Err("trace 未确认测速目标域名，响应内容不符合预期".into());
    }
    Ok(colo.to_string())
}

fn request_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        return "HTTPS 请求超时".into();
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
    if details.to_ascii_lowercase().contains("certificate") {
        format!("TLS 证书验证失败：{details}")
    } else {
        format!("HTTPS 请求失败：{details}")
    }
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
) -> Result<DownloadProbe, String> {
    let request_timeout = Duration::from_millis(config.request_timeout_ms)
        + Duration::from_secs(config.download_seconds);
    let client = client_for_ip(config, target, ip, request_timeout)?;
    let (url, requested_bytes) = match allowance {
        Some(bytes) => {
            let (url, bytes) = bounded_download_url(target, bytes);
            (url, Some(bytes))
        }
        None => (target.clone(), None),
    };
    let started = Instant::now();
    let mut request = client
        .get(url)
        .header(header::CONNECTION, "close")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ACCEPT_ENCODING, "identity");
    if let Some(bytes) = requested_bytes {
        request = request.header(header::RANGE, format!("bytes=0-{}", bytes - 1));
    }
    let response = request.send().await.map_err(request_error)?;
    measure_download_response(config, response, ip, requested_bytes, started, counter).await
}

async fn measure_download_response(
    config: &ScanConfig,
    mut response: Response,
    ip: Ipv4Addr,
    requested_bytes: Option<u64>,
    started: Instant,
    counter: Arc<std::sync::atomic::AtomicU64>,
) -> Result<DownloadProbe, String> {
    validate_response(&response, ip)?;
    if response
        .headers()
        .get(header::CONTENT_ENCODING)
        .is_some_and(|h| h != "identity")
    {
        return Err("下载响应进行了压缩，无法可靠比较原始下载吞吐".into());
    }
    if requested_bytes
        .is_some_and(|limit| response.content_length().is_none_or(|bytes| bytes > limit))
    {
        return Err(
            "下载目标未返回预算内的明确内容长度；请使用支持 Range 的静态文件或默认测速地址".into(),
        );
    }
    let expected_bytes = response.content_length();
    if expected_bytes.is_some_and(|bytes| bytes < MIN_SAMPLE_BYTES) {
        return Err("下载响应不足 64 KiB，样本过小；请使用较大的测试文件或增加数据预算".into());
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
        return Err("下载样本不足 64 KiB，时限内未获得有效测速数据".into());
    }
    // Include connection/TLS/header setup: this is effective single-request
    // throughput, consistently measured across all candidates.
    let mbps = received as f64 * 8.0 / started.elapsed().as_secs_f64().max(0.000001) / 1_000_000.0;
    Ok(DownloadProbe { mbps, colo })
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
    let value = headers.get("cf-ray")?.to_str().ok()?;
    let suffix = value.rsplit('-').next()?;
    (suffix.len() == 3 && suffix.bytes().all(|b| b.is_ascii_uppercase()))
        .then(|| suffix.to_string())
}

fn observe_colo(item: &mut IpResult, colo: &str) {
    if !item.observed_colos.iter().any(|c| c == colo) {
        item.observed_colos.push(colo.into());
    }
    if item.colo.is_empty() {
        item.colo = colo.into();
        if let Some((region, city)) = pop::location(colo) {
            item.region_code = region.into();
            item.city = city.into();
        }
    } else if colo != item.colo {
        match pop::location(colo) {
            Some((region, _)) if region != item.region_code => {
                item.exclusion_reason = Some(format!(
                    "接入地区发生变化：{} → {colo}，暂不作为固定地区推荐",
                    item.colo
                ));
            }
            None => {
                item.exclusion_reason = Some(format!("POP 变化至未知地区 {colo}，无法确认地区归属"))
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
        if result.exclusion_reason.is_none() && result.region_code != "UNKNOWN" {
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
                .filter(|(_, r)| r.region_code == *region && r.exclusion_reason.is_none())
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
                    results[*index].exclusion_reason.is_none()
                        && results[*index].stability_exclusion(config).is_none()
                })
                .collect();
            entries.sort_by(|a, b| {
                compare_ip_results_with_metric(
                    &results[*a],
                    &results[*b],
                    "daily",
                    &config.stability_latency_metric,
                )
            });
            if let Some(limit) = limit {
                for index in entries.iter().skip(limit) {
                    results[*index].exclusion_reason = Some(format!(
                        "TCP 稳定性已完成，未进入本地区前 {limit} 名下载候选（未测速）"
                    ));
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

fn download_schedule(
    groups: &BTreeMap<String, Vec<usize>>,
    region_order: &[String],
    rounds: usize,
) -> Vec<usize> {
    let max = groups.values().map(Vec::len).max().unwrap_or(0);
    let mut plan = Vec::new();
    // Complete the first candidate of every region before spending budget on
    // later backups. Every round rotates across regions to prevent starvation.
    for rank in 0..max {
        for _ in 0..rounds {
            for region in region_order {
                if let Some(index) = groups.get(region).and_then(|v| v.get(rank)) {
                    plan.push(*index);
                }
            }
        }
    }
    plan
}

fn rank_results(results: &mut [IpResult], config: &ScanConfig, completed: bool) {
    for result in results.iter_mut() {
        result.role = Some("candidate".into());
        result.measurement_passed = result.has_complete_measurement(config);
        result.eligible = result.measurement_passed;
        if result.eligible
            && config.preference == "daily"
            && result.download_mbps < config.min_download_mbps
        {
            result.eligible = false;
            result.exclusion_reason = Some(format!(
                "下载 {:.2} Mbps，低于日常优选目标 {:.2} Mbps",
                result.download_mbps, config.min_download_mbps
            ));
        } else if result.eligible && result.has_only_speed_target_exclusion() {
            // Reclassification may change the preference or target. This is
            // the only exclusion a passed measurement is allowed to clear.
            result.exclusion_reason = None;
        }
    }
    for region in &config.regions {
        let mut entries: Vec<_> = results
            .iter()
            .enumerate()
            .filter(|(_, r)| r.eligible && r.region_code == *region)
            .map(|(i, _)| i)
            .collect();
        entries.sort_by(|a, b| {
            compare_ip_results_with_metric(
                &results[*a],
                &results[*b],
                &config.preference,
                &config.stability_latency_metric,
            )
        });
        if completed {
            for (rank, index) in entries.into_iter().take(config.ips_per_region).enumerate() {
                results[index].role = Some(if rank == 0 { "primary" } else { "backup" }.into());
            }
        }
    }
}

fn median(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    if sorted.len() % 2 == 0 {
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
    let mut explicit = Vec::new();
    let mut networks = Vec::new();
    let mut seen_networks = HashSet::new();
    {
        let mut count = 0;
        for (line_number, line) in config.custom_ips.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("");
            for token in line
                .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
                .filter(|s| !s.is_empty())
            {
                count += 1;
                if count > 20000 {
                    return Err("自定义列表最多接受 20000 个 IP / 网段条目".into());
                }
                if token.contains('/') {
                    let net = token.parse::<Ipv4Net>().map_err(|_| {
                        format!("第 {} 行不是有效 IPv4 CIDR：{token}", line_number + 1)
                    })?;
                    if seen_networks.insert(net.trunc()) {
                        networks.push(net.trunc());
                    }
                } else {
                    let ip = token.parse::<Ipv4Addr>().map_err(|_| {
                        format!(
                            "第 {} 行不是有效 IPv4：{token}（第一版暂不支持 IPv6）",
                            line_number + 1
                        )
                    })?;
                    explicit.push(ip);
                }
            }
        }
    }
    let mut seen = HashSet::new();
    let mut output = Vec::new();
    for ip in explicit {
        if seen.insert(ip) {
            output.push(ip);
        }
        if output.len() >= config.max_candidates {
            return Ok(output);
        }
    }
    if !networks.is_empty() && output.len() < config.max_candidates {
        let remaining = config.max_candidates - output.len();
        let quota = remaining.div_ceil(networks.len());
        let mut rng = rand::thread_rng();
        for round in 0..quota {
            for network in &networks {
                let size = 1_u64 << (32 - network.prefix_len());
                let strata = (quota as u64).min(size);
                if round as u64 >= strata {
                    continue;
                }
                let start = round as u64 * size / strata;
                let end = (round as u64 + 1) * size / strata;
                let offset = rng.gen_range(start..end);
                let ip = Ipv4Addr::from((u64::from(u32::from(network.network())) + offset) as u32);
                if seen.insert(ip) {
                    output.push(ip);
                }
                if output.len() >= config.max_candidates {
                    return Ok(output);
                }
            }
        }
        // Overlapping ranges / small subnets may leave holes. Fill with bounded
        // attempts rather than enumerating any large address space.
        for round in 0..remaining.saturating_mul(8) {
            let network = &networks[round % networks.len()];
            let size = 1_u64 << (32 - network.prefix_len());
            let offset = rng.gen_range(0..size);
            let ip = Ipv4Addr::from((u64::from(u32::from(network.network())) + offset) as u32);
            if seen.insert(ip) {
                output.push(ip);
            }
            if output.len() >= config.max_candidates {
                break;
            }
        }
    }
    if output.is_empty() {
        return Err("候选列表为空，请输入 IPv4 或 CIDR".into());
    }
    Ok(output)
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
        Fut: std::future::Future<Output = Result<TraceProbe, String>>,
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
            std::future::ready(Err("连接失败".into()))
        })
        .await;
        assert_eq!(calls.get(), 1, "excluded IP continued service verification");
        assert_eq!(verified.attempts, 2);
        assert_eq!(verified.success_count, 1);
        assert_eq!(verified.https_ms, 70.0);
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
        let outcome = state.cancel(&config, &callback);
        let saved = &outcome.results[0];
        assert!(outcome.cancelled);
        assert_eq!(saved.attempts, 3, "cancelled request was counted");
        assert_eq!(saved.success_count, 3, "completed successes were discarded");
        assert_eq!(saved.https_ms, 30.0);
        assert_eq!(saved.jitter_ms, 50.0);
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
            let cancellation = CancellationToken::new();
            assert!(
                !record_download(
                    &mut row,
                    DownloadProbe {
                        mbps: 30.0,
                        colo: None
                    },
                    &config,
                    &cancellation,
                    || async {
                        cancellation.cancel();
                        std::future::pending().await
                    },
                )
                .await
            );
            let state = verification_state(vec![row]);
            let outcome = state.cancel(&config, &(Arc::new(|_| {}) as UpdateCallback));
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
            assert!(
                record_download(
                    &mut row,
                    DownloadProbe {
                        mbps: 30.0,
                        colo: None
                    },
                    &config,
                    &CancellationToken::new(),
                    || std::future::ready(
                        response
                            .map(|colo| TraceProbe {
                                https_ms: 60.0,
                                colo: colo.into()
                            })
                            .map_err(str::to_string)
                    ),
                )
                .await
            );
            assert_eq!(row.attempts, 6);
            assert_eq!(row.success_count, if response.is_ok() { 6 } else { 5 });
            assert_eq!(row.download_samples_mbps, vec![10.0, 30.0]);
            assert_eq!(row.download_mbps, 20.0);
            rank_results(std::slice::from_mut(&mut row), &config, true);
            let passed = response == Ok("HKG");
            assert_eq!(row.measurement_passed, passed);
            assert_eq!(row.eligible, passed);
            assert_eq!(row.exclusion_reason.is_none(), passed);
        }
    }

    #[tokio::test]
    async fn download_with_cf_ray_needs_no_additional_trace_request() {
        let config = ScanConfig::default();
        let mut row = measured("192.0.2.1", "HK", 70.0, 10.0);
        row.download_samples_mbps.truncate(1);
        let calls = std::cell::Cell::new(0);
        assert!(
            record_download(
                &mut row,
                DownloadProbe {
                    mbps: 30.0,
                    colo: Some("HKG".into())
                },
                &config,
                &CancellationToken::new(),
                || {
                    calls.set(calls.get() + 1);
                    std::future::ready(Err("unnecessary trace request".into()))
                },
            )
            .await
        );
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
    async fn unlimited_downloads_read_unknown_lengths_and_stop_at_the_round_deadline() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            // Unknown-length body that deliberately remains open after enough
            // data has arrived. Unlimited mode must enforce its time limit.
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nCF-Ray: abc-HKG\r\n\r\n")
                .await
                .unwrap();
            socket.write_all(&vec![b'x'; 128 * 1024]).await.unwrap();
            std::future::pending::<()>().await;
        });
        let response = Client::new()
            .get(format!("http://{address}/download"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.content_length(), None);
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let config = ScanConfig {
            download_budget_enabled: false,
            download_budget_mi_b: 0,
            download_seconds: 1,
            ..Default::default()
        };
        let started = Instant::now();
        let sample = tokio::time::timeout(
            Duration::from_millis(1800),
            measure_download_response(
                &config,
                response,
                Ipv4Addr::LOCALHOST,
                None,
                started,
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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
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
        assert!(error.contains("预算内的明确内容长度"));
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn low_budget_completes_some_front_candidates_instead_of_rejecting_every_slot() {
        let total = 1024 * 1024;
        let groups = BTreeMap::from([
            ("HK".into(), (0..10).collect()),
            ("JP".into(), (10..20).collect()),
            ("SG".into(), (20..30).collect()),
            ("US".into(), (30..40).collect()),
        ]);
        let order = vec!["HK".into(), "JP".into(), "SG".into(), "US".into()];
        let schedule = download_schedule(&groups, &order, 2);
        let mut samples = BTreeMap::<usize, usize>::new();
        let mut used = 0;
        for (slot, candidate) in schedule.iter().enumerate() {
            let allowance = download_allowance(total, used, (schedule.len() - slot) as u64);
            if allowance < MIN_SAMPLE_BYTES {
                break;
            }
            assert_eq!(allowance, MIN_SAMPLE_BYTES);
            used += allowance;
            *samples.entry(*candidate).or_default() += 1;
        }
        assert_eq!(used, total);
        assert_eq!(samples.len(), 8);
        assert!(samples.values().all(|rounds| *rounds == 2));
        assert_eq!(download_allowance(total, total - 1, 80), 1);
    }

    #[test]
    fn huge_cidr_materializes_only_the_limit() {
        let started = Instant::now();
        let samples = sample_candidates(&custom("0.0.0.0/0", 128)).unwrap();
        assert_eq!(samples.len(), 128);
        assert!(samples.iter().any(|ip| u32::from(*ip) < 1 << 31));
        assert!(samples.iter().any(|ip| u32::from(*ip) >= 1 << 31));
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
        assert_eq!(results[0].role.as_deref(), Some("candidate"));
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
                stability_batch: Some(StabilityBatch {
                    number: 2,
                    candidate_ips: vec![row.ip.clone()],
                }),
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
        let outcome = state.cancel(&ScanConfig::default(), &callback);
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
                stability_batch: Some(StabilityBatch {
                    number: 1,
                    candidate_ips: vec![row.ip.clone()],
                }),
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
                precision_batch: Some(StabilityBatch {
                    number: 3,
                    candidate_ips: vec!["192.0.2.1".into()],
                }),
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
        {
            let emitted = emitted.lock().unwrap();
            let first = emitted.last().unwrap();
            assert_eq!(first.progress.completed, 0);
            assert_eq!(first.progress.total, 100);
            let batch = first.progress.stability_batch.as_ref().unwrap();
            assert_eq!(batch.number, 2);
            assert_eq!(batch.candidate_ips, ["192.0.2.2"]);
            assert_eq!(
                first.results[0].tcp_stability.as_ref().unwrap().attempted,
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
        apply_stability_snapshot(
            &mut state,
            1,
            crate::stability::summarize(100, 230.0, vec![Some(10.0); 5]),
        );
        assert_eq!(state.progress.completed, 5);
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
        assert_eq!(
            download_schedule(&groups, &regions, 2),
            vec![0, 2, 3, 0, 2, 3, 1, 4, 1, 4]
        );
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
        headers.insert("cf-ray", "not-a-pop".parse().unwrap());
        assert_eq!(ray_colo(&headers), None);
    }

    #[tokio::test]
    async fn tcp_measures_a_real_local_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ms = tcp_probe(listener.local_addr().unwrap(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(ms.is_finite() && ms >= 0.0 && ms < 1000.0);
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
            client_builder_for_ip(
                &ScanConfig::default(),
                &self.target(),
                Ipv4Addr::LOCALHOST,
                Duration::from_secs(3),
            )
            .unwrap()
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
                let mut request = Vec::new();
                let mut buffer = [0_u8; 2048];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let length = tls.read(&mut buffer).await.map_err(|e| e.to_string())?;
                    if length == 0 {
                        return Err("client closed before sending HTTP headers".to_string());
                    }
                    request.extend_from_slice(&buffer[..length]);
                    if request.len() > 16 * 1024 {
                        return Err("oversized request headers".to_string());
                    }
                }
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
        assert!(validate_response(&response, Ipv4Addr::new(192, 0, 2, 1))
            .unwrap_err()
            .contains("连接对端发生变化"));
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
        let client = client_for_ip(
            &ScanConfig::default(),
            &target,
            Ipv4Addr::LOCALHOST,
            Duration::from_secs(3),
        )
        .unwrap();
        let error = client
            .get(target)
            .send()
            .await
            .expect_err("accepted self-signed certificate");
        assert!(request_error(error).contains("TLS 证书验证失败"));
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
        assert!(request_error(error).contains("TLS 证书验证失败"));
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
        assert!(reason.contains("重定向已拒绝"));
        // If redirects were enabled .send() would have tried the unresolvable
        // redirect target and failed instead of returning this original 302.
        assert_eq!(
            fixture.finish().await.unwrap().sni.as_deref(),
            Some(FIXTURE_HOST)
        );
    }
}

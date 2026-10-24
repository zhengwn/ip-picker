use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;

#[path = "engine/pop.rs"]
pub(crate) mod pop;

/// One randomly sampled host from every /24 in the built-in Cloudflare ranges.
pub const CLOUDFLARE_CANDIDATE_COUNT: usize = 5956;

/// All throughput values use decimal megabits per second (Mbps).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScanConfig {
    pub regions: Vec<String>,
    pub ips_per_region: usize,
    pub preference: String,
    pub min_download_mbps: f64,
    pub max_candidates: usize,
    pub tcp_concurrency: usize,
    #[serde(default = "default_max_tcp_ms")]
    pub max_tcp_ms: Option<f64>,
    pub tcp_latency_screening: bool,
    /// New scans finish stability for the retained regional pool, discard any
    /// candidate with two failures, and shortlist before downloading. Explicit
    /// false preserves the rules recorded by historical scans.
    pub stability_first: bool,
    #[serde(default = "default_stability_attempts")]
    pub stability_attempts: Option<usize>,
    pub stability_interval_ms: u64,
    pub max_stability_failure_percent: f64,
    pub stability_latency_metric: String,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    /// Adaptive mode uses the regional admission cap; limited mode additionally
    /// applies the manual candidates-per-region cap before stability testing.
    pub precision_mode: String,
    /// Keep only this many lowest initial TCP-latency candidates per region.
    /// Historical runs use explicit null to retain their former unbounded policy.
    pub regional_candidate_limit: Option<usize>,
    /// Legacy refill-pool size, retained for historical scans. Stability-first
    /// scans measure the full admitted pool and do not use this setting.
    pub candidate_pool_size: Option<usize>,
    pub candidates_per_region: usize,
    pub verification_attempts: usize,
    pub download_rounds: usize,
    pub download_seconds: u64,
    /// New scans default to unlimited data; storage preserves old run budgets.
    pub download_budget_enabled: bool,
    pub download_budget_mi_b: u64,
    pub source: String,
    pub custom_ips: String,
    pub target_url: String,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            regions: vec!["HK".into(), "JP".into(), "SG".into(), "US".into()],
            ips_per_region: 3,
            preference: "daily".into(),
            min_download_mbps: 20.0,
            max_candidates: CLOUDFLARE_CANDIDATE_COUNT,
            tcp_concurrency: 64,
            max_tcp_ms: default_max_tcp_ms(),
            tcp_latency_screening: false,
            stability_first: true,
            stability_attempts: default_stability_attempts(),
            stability_interval_ms: 200,
            max_stability_failure_percent: 1.0,
            stability_latency_metric: "p95".into(),
            connect_timeout_ms: 1500,
            request_timeout_ms: 5000,
            precision_mode: "adaptive".into(),
            regional_candidate_limit: Some(20),
            candidate_pool_size: Some(20),
            candidates_per_region: 10,
            verification_attempts: 5,
            download_rounds: 2,
            download_seconds: 3,
            download_budget_enabled: false,
            download_budget_mi_b: 400,
            source: "cloudflare".into(),
            custom_ips: String::new(),
            target_url: "https://speed.cloudflare.com/__down?bytes=25000000".into(),
        }
    }
}

impl ScanConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.stability_first {
            return Err("新测试须先完成 TCP 稳定性筛选，再选择每区前 10 个进入下载测速".into());
        }
        if self.tcp_latency_screening {
            return Err("新测试不使用单次 TCP 延迟淘汰，请使用连续 TCP 稳定性统计规则".into());
        }
        if self.regions.is_empty() || self.regions.len() > 32 {
            return Err("请选择 1 至 32 个目标国家／地区".into());
        }
        let mut seen = std::collections::HashSet::new();
        for region in &self.regions {
            if region.len() != 2 || !region.bytes().all(|b| b.is_ascii_uppercase()) {
                return Err(format!("地区代码必须是大写的两位代码：{region}"));
            }
            if !seen.insert(region) {
                return Err(format!("地区重复：{region}"));
            }
        }
        if !(1..=10).contains(&self.ips_per_region) {
            return Err("每区保留数量须为 1 至 10".into());
        }
        if !matches!(self.preference.as_str(), "daily" | "download") {
            return Err("未知推荐偏好".into());
        }
        if !self.min_download_mbps.is_finite() || !(0.0..=10000.0).contains(&self.min_download_mbps)
        {
            return Err("最低下载速度须为 0 至 10000 Mbps".into());
        }
        if !(1..=10000).contains(&self.max_candidates) {
            return Err("抽样数量须为 1 至 10000".into());
        }
        if !(1..=256).contains(&self.tcp_concurrency) {
            return Err("TCP 并发须为 1 至 256".into());
        }
        if !self
            .max_tcp_ms
            .is_some_and(|limit| limit.is_finite() && (1.0..=30000.0).contains(&limit))
        {
            return Err("请设置 1 至 30000 毫秒的 TCP 延迟上限".into());
        }
        if !self
            .stability_attempts
            .is_some_and(|count| (10..=1000).contains(&count))
        {
            return Err("TCP 稳定性探测次数须为 10 至 1000".into());
        }
        if !(50..=5000).contains(&self.stability_interval_ms) {
            return Err("TCP 稳定性探测间隔须为 50 至 5000 毫秒".into());
        }
        if !self.max_stability_failure_percent.is_finite()
            || !(0.0..=100.0).contains(&self.max_stability_failure_percent)
        {
            return Err("TCP 稳定性连接失败率上限须为 0 至 100%".into());
        }
        if !["average", "p95", "max"].contains(&self.stability_latency_metric.as_str()) {
            return Err("TCP 稳定性延迟规则须为 average、p95 或 max".into());
        }
        if !(100..=30000).contains(&self.connect_timeout_ms)
            || !(100..=60000).contains(&self.request_timeout_ms)
        {
            return Err("连接超时须为 100 至 30000 毫秒，请求超时须为 100 至 60000 毫秒".into());
        }
        if !matches!(self.precision_mode.as_str(), "adaptive" | "limited") {
            return Err("未知精测模式".into());
        }
        if !self
            .regional_candidate_limit
            .is_some_and(|limit| (self.ips_per_region..=10000).contains(&limit))
        {
            return Err("每区按初筛延迟保留的候选上限须不少于结果数量，且不超过 10000".into());
        }
        if self.precision_mode == "limited"
            && (self.candidates_per_region < self.ips_per_region
                || self.candidates_per_region > 10000)
        {
            return Err("手动精测上限须不少于保留数量，且不超过 10000".into());
        }
        if !(2..=10).contains(&self.verification_attempts) {
            return Err("HTTPS 验证次数须为 2 至 10".into());
        }
        if !(2..=5).contains(&self.download_rounds) {
            return Err("下载复测须为 2 至 5 轮".into());
        }
        if !(1..=15).contains(&self.download_seconds) {
            return Err("每轮下载时长须为 1 至 15 秒".into());
        }
        if self.download_budget_enabled && !(1..=4096).contains(&self.download_budget_mi_b) {
            return Err("下载数据预算须为 1 至 4096 MiB".into());
        }
        if !matches!(self.source.as_str(), "cloudflare" | "custom") {
            return Err("未知候选来源".into());
        }
        if self.custom_ips.len() > 2 * 1024 * 1024 {
            return Err("自定义列表不能超过 2 MiB".into());
        }
        if self.source == "custom" && self.custom_ips.trim().is_empty() {
            return Err("请输入或导入 IPv4 / CIDR 列表".into());
        }
        let url =
            url::Url::parse(&self.target_url).map_err(|_| "下载地址不是有效 URL".to_string())?;
        if url.scheme() != "https" {
            return Err("测速地址必须使用 HTTPS，以保留 TLS 证书验证".into());
        }
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err("测速地址不能包含用户名、密码或 URL 片段".into());
        }
        if !matches!(url.host(), Some(url::Host::Domain(_))) {
            return Err("测速地址须使用域名，以正确保留 TLS SNI 和证书校验".into());
        }
        if url.port_or_known_default() == Some(0) {
            return Err("测速端口不能为 0".into());
        }
        Ok(())
    }

    pub fn download_budget_bytes(&self) -> Option<u64> {
        self.download_budget_enabled
            .then(|| self.download_budget_mi_b.saturating_mul(1024 * 1024))
    }

    /// None denotes a historical run that did not record a TCP gate. New scan
    /// validation rejects None rather than silently starting an unlimited scan.
    pub fn accepts_tcp_ms(&self, milliseconds: f64) -> bool {
        milliseconds.is_finite()
            && milliseconds >= 0.0
            && self
                .max_tcp_ms
                .is_none_or(|limit| limit.is_finite() && milliseconds <= limit)
    }

    /// Initial connection timing remains an observation. Only historical runs
    /// that explicitly used single-probe screening apply the latency limit here.
    pub fn accepts_initial_tcp_ms(&self, milliseconds: f64) -> bool {
        milliseconds.is_finite()
            && milliseconds >= 0.0
            && (!self.tcp_latency_screening || self.accepts_tcp_ms(milliseconds))
    }
}

fn default_max_tcp_ms() -> Option<f64> {
    Some(230.0)
}

fn default_stability_attempts() -> Option<usize> {
    Some(100)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StabilityBatch {
    pub number: usize,
    pub candidate_ips: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    pub stage: String,
    pub completed: usize,
    pub total: usize,
    pub downloaded_bytes: u64,
    pub discovered_by_region: BTreeMap<String, usize>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_path: Option<crate::network::NetworkPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stability_batch: Option<StabilityBatch>,
    /// The candidates participating in the current precision batch, including
    /// candidates waiting for validation/download, not only active requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision_batch: Option<StabilityBatch>,
}

impl Default for ScanProgress {
    fn default() -> Self {
        Self {
            stage: "connecting".into(),
            completed: 0,
            total: 0,
            downloaded_bytes: 0,
            discovered_by_region: BTreeMap::new(),
            message: "准备候选地址".into(),
            network_path: None,
            stability_batch: None,
            precision_batch: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpResult {
    pub ip: String,
    pub region_code: String,
    pub city: String,
    pub colo: String,
    pub observed_colos: Vec<String>,
    pub tcp_ms: f64,
    pub https_ms: f64,
    /// Range (maximum - minimum) of successful HTTPS header timings.
    pub jitter_ms: f64,
    pub download_mbps: f64,
    pub success_count: usize,
    pub attempts: usize,
    pub download_samples_mbps: Vec<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_stability: Option<crate::stability::TcpStability>,
    /// Full HTTPS/download verification passed. Independent of the configured
    /// daily speed target; false for failed, drifting, or incomplete candidates.
    #[serde(default)]
    pub measurement_passed: bool,
    pub eligible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclusion_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl IpResult {
    pub fn new(ip: std::net::Ipv4Addr) -> Self {
        Self {
            ip: ip.to_string(),
            region_code: "UNKNOWN".into(),
            city: String::new(),
            colo: String::new(),
            observed_colos: Vec::new(),
            tcp_ms: 0.0,
            https_ms: 0.0,
            jitter_ms: 0.0,
            download_mbps: 0.0,
            success_count: 0,
            attempts: 0,
            download_samples_mbps: Vec::new(),
            tcp_stability: None,
            measurement_passed: false,
            eligible: false,
            exclusion_reason: None,
            role: Some("candidate".into()),
        }
    }

    /// Common evidence check for new scans and legacy recovery.
    /// A speed-target-only exclusion is a recommendation decision, not a failed
    /// measurement. Any other exclusion remains disqualifying.
    pub fn has_complete_measurement(&self, config: &ScanConfig) -> bool {
        self.success_count == self.attempts
            && self.attempts >= config.verification_attempts
            && self.download_samples_mbps.len() >= config.download_rounds
            && self
                .download_samples_mbps
                .iter()
                .all(|value| value.is_finite() && *value > 0.0)
            && self.download_mbps.is_finite()
            && self.download_mbps > 0.0
            && self.tcp_ms.is_finite()
            && self.tcp_ms >= 0.0
            && config.accepts_initial_tcp_ms(self.tcp_ms)
            && (config.stability_attempts.is_none()
                || self
                    .tcp_stability
                    .as_ref()
                    .is_some_and(|stats| stats.completed))
            && self.stability_exclusion(config).is_none()
            && self.https_ms.is_finite()
            && self.https_ms >= 0.0
            && self.jitter_ms.is_finite()
            && self.jitter_ms >= 0.0
            && self.colo.len() == 3
            && self.colo.bytes().all(|value| value.is_ascii_uppercase())
            && !self.observed_colos.is_empty()
            && self.observed_colos.contains(&self.colo)
            && pop::location(&self.colo).is_some_and(|(region, _)| region == self.region_code)
            && self.observed_colos.iter().all(|colo| {
                pop::location(colo).is_some_and(|(region, _)| region == self.region_code)
            })
            && config.regions.contains(&self.region_code)
            && (self.exclusion_reason.is_none() || self.has_only_speed_target_exclusion())
    }

    /// Match the exact legacy speed-filter format, never a substring in another
    /// network/TLS failure. Keep this parser shared by storage and the engine.
    pub fn has_only_speed_target_exclusion(&self) -> bool {
        let Some(reason) = self.exclusion_reason.as_deref() else {
            return false;
        };
        let Some(values) = reason.strip_prefix("下载 ") else {
            return false;
        };
        let Some((speed, target)) = values.split_once(" Mbps，低于日常优选目标 ") else {
            return false;
        };
        let Some(target) = target.strip_suffix(" Mbps") else {
            return false;
        };
        let numeric = |value: &str| {
            value
                .parse::<f64>()
                .is_ok_and(|number| number.is_finite() && number >= 0.0)
        };
        numeric(speed) && numeric(target)
    }

    pub fn stability_exclusion(&self, config: &ScanConfig) -> Option<String> {
        let requested = config.stability_attempts?;
        let Some(stats) = self.tcp_stability.as_ref() else {
            return Some(format!("TCP 稳定性探测未完成：0 / {requested}"));
        };
        if !stats.completed
            || stats.requested != requested
            || stats.attempted != requested
            || stats.probes_ms.len() != requested
            || stats.succeeded.checked_add(stats.failed) != Some(stats.attempted)
        {
            // The stop marker describes execution, not evidence. An imported
            // or stale flag can never stand in for the actual raw-sample proof.
            // Cancellation without this marker remains an unfinished probe.
            if stats.early_rejection.is_some()
                && stats.requested == requested
                && stats.attempted == stats.probes_ms.len()
                && stats.attempted < requested
                && stats.succeeded == stats.probes_ms.iter().flatten().count()
                && stats.failed
                    == stats
                        .probes_ms
                        .iter()
                        .filter(|probe| probe.is_none())
                        .count()
            {
                let rejection = if config.stability_first {
                    crate::stability::failure_count_exclusion(&stats.probes_ms)
                        .map(|reason| format!("TCP 稳定性提前淘汰：{reason}"))
                } else {
                    config.max_tcp_ms.and_then(|limit| {
                        crate::stability::impossible_to_pass(
                            &stats.probes_ms,
                            requested,
                            limit,
                            config.max_stability_failure_percent,
                            &config.stability_latency_metric,
                        )
                    })
                };
                if let Some(reason) = rejection {
                    return Some(reason);
                }
            }
            return Some(format!(
                "TCP 稳定性探测未完成：{} / {requested}",
                stats.attempted
            ));
        }
        let succeeded = stats
            .probes_ms
            .iter()
            .filter(|probe| probe.is_some())
            .count();
        if succeeded != stats.succeeded
            || requested - succeeded != stats.failed
            || stats
                .probes_ms
                .iter()
                .flatten()
                .any(|ms| !ms.is_finite() || *ms < 0.0)
        {
            return Some("TCP 稳定性统计不完整，不能参与推荐".into());
        }
        if succeeded == 0 {
            return Some("TCP 稳定性探测全部连接失败".into());
        }
        if config.stability_first {
            if let Some(reason) = crate::stability::failure_count_exclusion(&stats.probes_ms) {
                return Some(format!("TCP 稳定性{reason}"));
            }
        }
        let selected =
            crate::stability::latency_metric_ms(&stats.probes_ms, &config.stability_latency_metric);
        let Some(selected) = selected else {
            return Some("TCP 稳定性延迟统计不完整，不能参与推荐".into());
        };
        if !config.accepts_tcp_ms(selected) {
            let label = match config.stability_latency_metric.as_str() {
                "average" => "平均",
                "max" => "最大",
                _ => "P95",
            };
            return Some(format!(
                "TCP 稳定性{label}耗时 {selected} ms，超过上限 {} ms",
                config.max_tcp_ms.unwrap_or(stats.delay_limit_ms)
            ));
        }
        let stored_selected = match config.stability_latency_metric.as_str() {
            "average" => stats.average_ms,
            "max" => stats.max_ms,
            _ => stats.p95_ms,
        };
        if !stored_selected.is_some_and(|value| {
            value.is_finite()
                && (value - selected).abs() <= f64::EPSILON * selected.abs().max(1.0) * 8.0
        }) {
            return Some("TCP 稳定性所选统计与原始样本不一致，不能参与推荐".into());
        }
        if [
            stats.average_ms,
            stats.median_ms,
            stats.min_ms,
            stats.max_ms,
            stats.p95_ms,
        ]
        .iter()
        .any(|ms| !ms.is_some_and(|value| value.is_finite() && value >= 0.0))
            || !stats
                .failure_percent
                .is_some_and(|value| value.is_finite() && (0.0..=100.0).contains(&value))
        {
            return Some("TCP 稳定性耗时统计不完整，不能参与推荐".into());
        }
        let failure_percent = stats.failed as f64 * 100.0 / requested as f64;
        if failure_percent > config.max_stability_failure_percent {
            return Some(format!(
                "TCP 稳定性连接失败率 {failure_percent}%，超过上限 {}%",
                config.max_stability_failure_percent
            ));
        }
        None
    }
}

pub fn tcp_display_ms(result: &IpResult) -> f64 {
    tcp_display_ms_with_metric(result, "p95")
}

pub fn tcp_display_ms_with_metric(result: &IpResult, metric: &str) -> f64 {
    result
        .tcp_stability
        .as_ref()
        .filter(|stats| stats.completed && stats.attempted == stats.requested)
        .and_then(|stats| crate::stability::latency_metric_ms(&stats.probes_ms, metric))
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
        .unwrap_or(result.tcp_ms)
}

pub fn tcp_failure_percent(result: &IpResult) -> f64 {
    result
        .tcp_stability
        .as_ref()
        .filter(|stats| {
            stats.completed && stats.attempted == stats.requested && stats.attempted > 0
        })
        .map(|stats| stats.failed as f64 * 100.0 / stats.attempted as f64)
        .unwrap_or(f64::INFINITY)
}

/// Shared preference order used by recommendations and result displays.
pub fn compare_ip_results(a: &IpResult, b: &IpResult, preference: &str) -> Ordering {
    compare_ip_results_with_metric(a, b, preference, "p95")
}

pub fn compare_ip_results_with_metric(
    a: &IpResult,
    b: &IpResult,
    preference: &str,
    metric: &str,
) -> Ordering {
    if preference == "download" {
        b.download_mbps
            .total_cmp(&a.download_mbps)
            .then(tcp_failure_percent(a).total_cmp(&tcp_failure_percent(b)))
            .then(
                tcp_display_ms_with_metric(a, metric)
                    .total_cmp(&tcp_display_ms_with_metric(b, metric)),
            )
            .then(a.https_ms.total_cmp(&b.https_ms))
            .then(a.jitter_ms.total_cmp(&b.jitter_ms))
            .then(a.ip.cmp(&b.ip))
    } else {
        tcp_failure_percent(a)
            .total_cmp(&tcp_failure_percent(b))
            .then(
                tcp_display_ms_with_metric(a, metric)
                    .total_cmp(&tcp_display_ms_with_metric(b, metric)),
            )
            .then(a.https_ms.total_cmp(&b.https_ms))
            .then(a.jitter_ms.total_cmp(&b.jitter_ms))
            .then(b.download_mbps.total_cmp(&a.download_mbps))
            .then(a.ip.cmp(&b.ip))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanRun {
    pub id: String,
    pub status: String,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub config: ScanConfig,
    pub progress: ScanProgress,
    pub results: Vec<IpResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        ScanConfig::default().validate().unwrap();
        assert!(!ScanConfig::default().download_budget_enabled);
    }

    #[test]
    fn missing_budget_setting_defaults_to_unlimited_and_ignores_its_unused_value() {
        let omitted: ScanConfig = serde_json::from_value(serde_json::json!({
            "downloadBudgetMiB": 123
        }))
        .unwrap();
        assert!(!omitted.download_budget_enabled);
        assert_eq!(omitted.download_budget_bytes(), None);
        assert_eq!(
            serde_json::to_value(&omitted).unwrap()["downloadBudgetEnabled"],
            false
        );
        let enabled: ScanConfig = serde_json::from_value(serde_json::json!({
            "downloadBudgetEnabled": true,
            "downloadBudgetMiB": 123
        }))
        .unwrap();
        assert_eq!(enabled.download_budget_bytes(), Some(123 * 1024 * 1024));
        for value in [0, 4097, u64::MAX] {
            let mut config = ScanConfig {
                download_budget_enabled: false,
                download_budget_mi_b: value,
                ..Default::default()
            };
            config.validate().unwrap();
            assert_eq!(config.download_budget_bytes(), None);
            config.download_budget_enabled = true;
            assert!(config.validate().is_err());
        }
        // Removing the byte cap does not remove time and round limits.
        for (seconds, rounds) in [(0, 2), (16, 2), (3, 0), (3, 6)] {
            assert!(ScanConfig {
                download_budget_enabled: false,
                download_seconds: seconds,
                download_rounds: rounds,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
    }

    #[test]
    fn new_scans_require_stability_before_the_download_shortlist() {
        let config: ScanConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(config.stability_first);
        assert_eq!(
            serde_json::to_value(&config).unwrap()["stabilityFirst"],
            true
        );
        assert!(ScanConfig {
            stability_first: false,
            ..config
        }
        .validate()
        .is_err());
    }

    #[test]
    fn adaptive_precision_is_default_and_does_not_apply_the_manual_candidate_cap() {
        let config: ScanConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(config.precision_mode, "adaptive");
        ScanConfig {
            candidates_per_region: 0,
            ..config
        }
        .validate()
        .unwrap();
        for cap in [3, 10, 10000] {
            ScanConfig {
                precision_mode: "limited".into(),
                candidates_per_region: cap,
                ..Default::default()
            }
            .validate()
            .unwrap();
        }
        for cap in [0, 2, 10001] {
            assert!(ScanConfig {
                precision_mode: "limited".into(),
                candidates_per_region: cap,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        assert!(ScanConfig {
            precision_mode: "unknown".into(),
            ..Default::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn regional_latency_queue_defaults_to_twenty_and_must_be_bounded_for_new_scans() {
        let config: ScanConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(config.regional_candidate_limit, Some(20));
        for limit in [Some(3), Some(50), Some(10000)] {
            ScanConfig {
                regional_candidate_limit: limit,
                ..Default::default()
            }
            .validate()
            .unwrap();
        }
        for limit in [None, Some(0), Some(2), Some(10001)] {
            assert!(ScanConfig {
                regional_candidate_limit: limit,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        let historical: ScanConfig = serde_json::from_value(serde_json::json!({
            "regionalCandidateLimit": null
        }))
        .unwrap();
        assert_eq!(historical.regional_candidate_limit, None);
        assert_eq!(
            serde_json::to_value(historical).unwrap()["regionalCandidateLimit"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn legacy_refill_pool_size_does_not_limit_stability_first_scans() {
        let config: ScanConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(config.candidate_pool_size, Some(20));
        for size in [None, Some(0), Some(2), Some(3), Some(20), Some(1001)] {
            ScanConfig {
                ips_per_region: 10,
                candidate_pool_size: size,
                ..Default::default()
            }
            .validate()
            .unwrap();
        }
        let historical: ScanConfig =
            serde_json::from_value(serde_json::json!({"candidatePoolSize": null})).unwrap();
        assert_eq!(historical.candidate_pool_size, None);
        ScanConfig {
            precision_mode: "limited".into(),
            ..historical
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn missing_new_tcp_gate_defaults_to_230_but_explicit_null_cannot_start_a_scan() {
        let config: ScanConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(config.max_tcp_ms, Some(230.0));
        config.validate().unwrap();
        let historical: ScanConfig =
            serde_json::from_value(serde_json::json!({"maxTcpMs": null})).unwrap();
        assert_eq!(historical.max_tcp_ms, None);
        assert!(historical.validate().is_err());
    }

    #[test]
    fn tcp_gate_validation_rejects_nonfinite_or_out_of_range_values() {
        for limit in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, 30000.001] {
            assert!(ScanConfig {
                max_tcp_ms: Some(limit),
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for limit in [1.0, 200.0, 30000.0] {
            ScanConfig {
                max_tcp_ms: Some(limit),
                ..Default::default()
            }
            .validate()
            .unwrap();
        }
        assert!(ScanConfig::default().accepts_tcp_ms(230.0));
        assert!(!ScanConfig::default().accepts_tcp_ms(230.001));
        assert!(!ScanConfig::default().accepts_tcp_ms(f64::NAN));
    }

    #[test]
    fn new_stability_defaults_are_explicit_while_null_only_represents_history() {
        let config: ScanConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(config.stability_attempts, Some(100));
        assert_eq!(config.stability_interval_ms, 200);
        assert_eq!(config.max_stability_failure_percent, 1.0);
        assert_eq!(config.stability_latency_metric, "p95");
        config.validate().unwrap();
        let legacy: ScanConfig =
            serde_json::from_value(serde_json::json!({"stabilityAttempts":null})).unwrap();
        assert!(legacy.stability_attempts.is_none());
        assert!(legacy.validate().is_err());
        for count in [9, 1001] {
            assert!(ScanConfig {
                stability_attempts: Some(count),
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for interval in [49, 5001] {
            assert!(ScanConfig {
                stability_interval_ms: interval,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for percent in [f64::NAN, f64::INFINITY, -0.01, 100.01] {
            assert!(ScanConfig {
                max_stability_failure_percent: percent,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
    }

    #[test]
    fn rejects_insecure_or_ambiguous_target() {
        for target in [
            "http://example.com/file",
            "https://1.1.1.1/file",
            "https://a:b@example.com/file",
            "https://example.com/file#fragment",
        ] {
            let config = ScanConfig {
                target_url: target.into(),
                ..Default::default()
            };
            assert!(config.validate().is_err(), "accepted {target}");
        }
    }

    #[test]
    fn rejects_unbounded_or_incomplete_runs() {
        let config = ScanConfig {
            max_candidates: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config = ScanConfig {
            download_rounds: 1,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config = ScanConfig {
            regions: vec!["HK".into(), "HK".into()],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn json_contract_preserves_mib_spelling_and_roundtrips() {
        let value = serde_json::to_value(ScanConfig::default()).unwrap();
        assert_eq!(value["downloadBudgetMiB"], 400);
        assert!(value.get("downloadBudgetMib").is_none());
        assert_eq!(value["verificationAttempts"], 5);
        assert_eq!(value["precisionMode"], "adaptive");
        assert_eq!(value["regionalCandidateLimit"], 20);
        assert_eq!(value["candidatePoolSize"], 20);
        let decoded: ScanConfig = serde_json::from_value(value).unwrap();
        decoded.validate().unwrap();
    }

    #[test]
    fn empty_measurements_are_finite_and_json_safe() {
        let item = IpResult::new(std::net::Ipv4Addr::LOCALHOST);
        for value in [
            item.tcp_ms,
            item.https_ms,
            item.jitter_ms,
            item.download_mbps,
        ] {
            assert!(value.is_finite());
        }
        let json = serde_json::to_value(item).unwrap();
        assert_eq!(json["regionCode"], "UNKNOWN");
        assert_eq!(json["eligible"], false);
        assert_eq!(json["measurementPassed"], false);
        assert_eq!(json["downloadSamplesMbps"], serde_json::json!([]));
    }

    #[test]
    fn legacy_ip_result_without_measurement_passed_defaults_to_false() {
        let mut json = serde_json::to_value(IpResult::new(std::net::Ipv4Addr::LOCALHOST)).unwrap();
        json.as_object_mut().unwrap().remove("measurementPassed");
        let result: IpResult = serde_json::from_value(json).unwrap();
        assert!(!result.measurement_passed);
    }

    #[test]
    fn latency_metric_defaults_to_p95_and_accepts_only_the_three_explicit_policies() {
        let config: ScanConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(config.stability_latency_metric, "p95");
        for metric in ["average", "p95", "max"] {
            ScanConfig {
                stability_latency_metric: metric.into(),
                ..Default::default()
            }
            .validate()
            .unwrap();
        }
        for metric in ["", "P95", "median"] {
            assert!(ScanConfig {
                stability_latency_metric: metric.into(),
                ..Default::default()
            }
            .validate()
            .is_err());
        }
    }

    #[test]
    fn stability_batch_json_is_optional_for_old_progress_and_uses_camel_case() {
        let old = serde_json::to_value(ScanProgress::default()).unwrap();
        assert!(old.get("stabilityBatch").is_none());
        assert!(old.get("precisionBatch").is_none());
        assert!(serde_json::from_value::<ScanProgress>(old)
            .unwrap()
            .stability_batch
            .is_none());
        let progress = ScanProgress {
            stage: "stabilizing".into(),
            stability_batch: Some(StabilityBatch {
                number: 2,
                candidate_ips: vec!["192.0.2.1".into()],
            }),
            precision_batch: Some(StabilityBatch {
                number: 2,
                candidate_ips: vec!["192.0.2.1".into(), "192.0.2.2".into()],
            }),
            ..Default::default()
        };
        let json = serde_json::to_value(progress).unwrap();
        assert_eq!(json["stabilityBatch"]["number"], 2);
        assert_eq!(json["stabilityBatch"]["candidateIps"][0], "192.0.2.1");
        assert_eq!(json["precisionBatch"]["candidateIps"][1], "192.0.2.2");
    }

    #[test]
    fn early_rejection_reason_is_recomputed_and_never_trusted_from_a_marker() {
        let config = ScanConfig::default();
        let mut row = IpResult::new(std::net::Ipv4Addr::LOCALHOST);
        let mut stats = crate::stability::summarize(100, 230.0, vec![None, None]);
        stats.early_rejection = Some("untrusted saved reason".into());
        row.tcp_stability = Some(stats);
        let reason = row.stability_exclusion(&config).unwrap();
        assert!(reason.contains("提前淘汰"));
        assert!(reason.contains("累计 2 次连接失败"));
        assert!(!reason.contains("untrusted"));
        assert!(!row.has_complete_measurement(&config));

        let stats = row.tcp_stability.as_mut().unwrap();
        stats.early_rejection = None;
        assert!(row.stability_exclusion(&config).unwrap().contains("未完成"));

        let mut forged = crate::stability::summarize(100, 230.0, vec![Some(20.0); 10]);
        forged.early_rejection = Some("TCP 稳定性提前淘汰：forged".into());
        row.tcp_stability = Some(forged);
        assert!(row.stability_exclusion(&config).unwrap().contains("未完成"));

        let stats = row.tcp_stability.as_mut().unwrap();
        stats.completed = true;
        assert!(row.stability_exclusion(&config).unwrap().contains("未完成"));
        assert!(!row.has_complete_measurement(&config));
    }

    #[test]
    fn partial_early_rejection_revalidates_raw_counts_and_the_actual_latency_policy() {
        let config = ScanConfig {
            stability_first: false,
            ..Default::default()
        };
        let mut row = IpResult::new(std::net::Ipv4Addr::LOCALHOST);
        let mut stats = crate::stability::summarize(100, 230.0, vec![Some(231.0); 6]);
        stats.early_rejection = Some("stop".into());
        row.tcp_stability = Some(stats);
        assert!(row
            .stability_exclusion(&config)
            .unwrap()
            .contains("最终 P95"));

        let average = ScanConfig {
            stability_latency_metric: "average".into(),
            ..config.clone()
        };
        assert!(row
            .stability_exclusion(&average)
            .unwrap()
            .contains("未完成"));
        let stats = row.tcp_stability.as_mut().unwrap();
        stats.succeeded += 1;
        assert!(row.stability_exclusion(&config).unwrap().contains("未完成"));
    }

    #[test]
    fn current_policy_does_not_accept_latency_alone_as_an_early_rejection_proof() {
        let config = ScanConfig::default();
        let mut row = IpResult::new(std::net::Ipv4Addr::LOCALHOST);
        let mut stats = crate::stability::summarize(100, 230.0, vec![Some(231.0); 6]);
        stats.early_rejection = Some("forged latency rejection".into());
        row.tcp_stability = Some(stats);
        assert!(row.stability_exclusion(&config).unwrap().contains("未完成"));
    }

    #[test]
    fn current_policy_rejects_two_failures_even_at_the_last_probe_with_a_permissive_rate() {
        let config = ScanConfig {
            max_stability_failure_percent: 100.0,
            ..Default::default()
        };
        let mut row = IpResult::new(std::net::Ipv4Addr::LOCALHOST);
        let mut probes = vec![Some(20.0); 98];
        probes.extend([None, None]);
        row.tcp_stability = Some(crate::stability::summarize(100, 230.0, probes));
        assert!(row.tcp_stability.as_ref().unwrap().completed);
        assert!(row
            .tcp_stability
            .as_ref()
            .unwrap()
            .early_rejection
            .is_none());
        assert!(row
            .stability_exclusion(&config)
            .unwrap()
            .contains("累计 2 次连接失败"));
        let historical = ScanConfig {
            stability_first: false,
            ..config
        };
        assert!(row.stability_exclusion(&historical).is_none());
    }

    #[test]
    fn new_scans_cannot_enable_the_legacy_single_connection_screen() {
        let config: ScanConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!config.tcp_latency_screening);
        assert!(config.accepts_initial_tcp_ms(350.0));
        assert!(config.accepts_initial_tcp_ms(600.0));
        assert!(!config.accepts_initial_tcp_ms(f64::NAN));
        assert!(!config.accepts_initial_tcp_ms(-1.0));
        config.validate().unwrap();
        let historical = ScanConfig {
            tcp_latency_screening: true,
            max_tcp_ms: Some(200.0),
            ..Default::default()
        };
        assert!(historical.validate().is_err());
        assert!(!historical.accepts_initial_tcp_ms(200.001));
        assert!(historical.accepts_initial_tcp_ms(200.0));
    }

    #[test]
    fn only_the_exact_numeric_speed_target_reason_is_recoverable() {
        let mut result = IpResult::new(std::net::Ipv4Addr::LOCALHOST);
        result.exclusion_reason = Some("下载 8.40 Mbps，低于日常优选目标 20.00 Mbps".into());
        assert!(result.has_only_speed_target_exclusion());
        // Rounded values can coincide even though the raw value is just below
        // its target. Identification is based on the exact generated format.
        result.exclusion_reason = Some("下载 20.00 Mbps，低于日常优选目标 20.00 Mbps".into());
        assert!(result.has_only_speed_target_exclusion());
        for reason in [
            "HTTPS 请求失败：下载 8.40 Mbps，低于日常优选目标 20.00 Mbps",
            "下载 8.40 Mbps，低于日常优选目标 20.00 Mbps；TLS 验证失败",
            "下载 NaN Mbps，低于日常优选目标 20.00 Mbps",
            "下载 -8.40 Mbps，低于日常优选目标 20.00 Mbps",
            "下载预算不足，未完成必要复测",
        ] {
            result.exclusion_reason = Some(reason.into());
            assert!(
                !result.has_only_speed_target_exclusion(),
                "accepted {reason}"
            );
        }
    }
}

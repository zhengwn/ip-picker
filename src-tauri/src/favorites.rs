use crate::{
    model::{IpResult, ScanConfig, ScanRun},
    network::NetworkPath,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, net::Ipv4Addr};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteTestJob {
    pub run: ScanRun,
    pub favorite_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteTestStart {
    pub favorites: FavoriteLibrary,
    pub job: FavoriteTestJob,
}

impl FavoriteTestJob {
    /// The targeted engine accepts observed countries outside the discovery
    /// filter. Record those countries alongside the evidence for later gating.
    pub fn record_observed_regions(&mut self) {
        for result in &self.run.results {
            let region = &result.region_code;
            if region.len() == 2
                && region.bytes().all(|byte| byte.is_ascii_uppercase())
                && !self.run.config.regions.contains(region)
            {
                self.run.config.regions.push(region.clone());
            }
        }
    }

    pub fn interrupt(&mut self, message: &str) {
        self.run.status = "cancelled".into();
        self.run.finished_at = Some(chrono::Utc::now().to_rfc3339());
        self.run.progress.stage = "cancelled".into();
        self.run.progress.stability_batch = None;
        self.run.progress.precision_batch = None;
        self.run.progress.message = message.into();
        for result in &mut self.run.results {
            result.role = Some("candidate".into());
        }
        self.record_observed_regions();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteFolder {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteEntry {
    pub id: String,
    pub folder_id: String,
    pub saved_at: String,
    pub note: String,
    pub run_id: String,
    pub run_status: String,
    pub run_started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_finished_at: Option<String>,
    pub config: ScanConfig,
    pub result: IpResult,
    #[serde(default)]
    pub targeted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_path: Option<NetworkPath>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteLibrary {
    /// Monotonic, persisted version of an atomically committed whole-library snapshot.
    #[serde(default)]
    pub revision: u64,
    pub folders: Vec<FavoriteFolder>,
    pub entries: Vec<FavoriteEntry>,
}

impl Default for FavoriteLibrary {
    fn default() -> Self {
        let created_at = chrono::Utc::now().to_rfc3339();
        Self {
            revision: 0,
            folders: ["联通流量", "联通宽带", "移动流量"]
                .into_iter()
                .map(|name| FavoriteFolder {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: name.into(),
                    created_at: created_at.clone(),
                })
                .collect(),
            entries: Vec::new(),
        }
    }
}

impl FavoriteLibrary {
    /// Allocate only at a saved-state commit boundary, never for an in-progress
    /// candidate edit. Keep JSON integers exact for the JavaScript UI guard.
    pub fn next_revision(&self) -> Result<u64, String> {
        self.revision
            .checked_add(1)
            .filter(|revision| *revision <= 9_007_199_254_740_991)
            .ok_or_else(|| "收藏库修订号已达到安全上限，无法保存新的修改".into())
    }

    fn folder_index(&self, folder_id: &str) -> Result<usize, String> {
        self.folders
            .iter()
            .position(|folder| folder.id == folder_id)
            .ok_or_else(|| "收藏夹不存在，请刷新后重试".into())
    }

    fn checked_name(&self, name: &str, except_id: Option<&str>) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 40 {
            return Err("收藏夹名称需为 1–40 个字符".into());
        }
        if self
            .folders
            .iter()
            .any(|folder| Some(folder.id.as_str()) != except_id && folder.name == name)
        {
            return Err("已有同名收藏夹，请使用其他名称".into());
        }
        Ok(name.into())
    }

    pub fn create_folder(&mut self, name: &str) -> Result<(), String> {
        let name = self.checked_name(name, None)?;
        self.folders.push(FavoriteFolder {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            created_at: chrono::Utc::now().to_rfc3339(),
        });
        Ok(())
    }

    pub fn rename_folder(&mut self, folder_id: &str, name: &str) -> Result<(), String> {
        let index = self.folder_index(folder_id)?;
        let name = self.checked_name(name, Some(folder_id))?;
        self.folders[index].name = name;
        Ok(())
    }

    pub fn delete_folder(&mut self, folder_id: &str) -> Result<(), String> {
        let index = self.folder_index(folder_id)?;
        self.folders.remove(index);
        self.entries.retain(|entry| entry.folder_id != folder_id);
        Ok(())
    }

    /// Capture the authoritative row and its original test context together.
    /// These values are never recomputed using settings from a later test.
    pub fn save_snapshot(
        &mut self,
        folder_id: &str,
        run: &ScanRun,
        ip: &str,
        note: &str,
    ) -> Result<(), String> {
        self.folder_index(folder_id)?;
        let note = checked_note(note)?;
        let result = run
            .results
            .iter()
            .find(|result| result.ip == ip)
            .ok_or("该 IP 不在本次测试结果中，请刷新后重试")?;
        let existing = self
            .entries
            .iter()
            .position(|entry| entry.folder_id == folder_id && entry.result.ip == ip);
        let snapshot = FavoriteEntry {
            id: existing
                .map(|index| self.entries[index].id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            folder_id: folder_id.into(),
            saved_at: chrono::Utc::now().to_rfc3339(),
            note,
            run_id: run.id.clone(),
            run_status: run.status.clone(),
            run_started_at: run.started_at.clone(),
            run_finished_at: run.finished_at.clone(),
            config: run.config.clone(),
            result: result.clone(),
            targeted: false,
            network_path: run.progress.network_path.clone(),
        };
        if let Some(index) = existing {
            self.entries[index] = snapshot;
        } else {
            self.entries.push(snapshot);
        }
        Ok(())
    }

    pub fn update_entry(
        &mut self,
        favorite_id: &str,
        folder_id: &str,
        note: &str,
    ) -> Result<(), String> {
        self.folder_index(folder_id)?;
        let note = checked_note(note)?;
        let index = self
            .entries
            .iter()
            .position(|entry| entry.id == favorite_id)
            .ok_or("收藏记录不存在，请刷新后重试")?;
        let ip = &self.entries[index].result.ip;
        if self.entries.iter().any(|entry| {
            entry.id != favorite_id && entry.folder_id == folder_id && entry.result.ip == *ip
        }) {
            return Err("目标收藏夹已有该 IP，请先删除其中一条记录再移动".into());
        }
        self.entries[index].folder_id = folder_id.into();
        self.entries[index].note = note;
        Ok(())
    }

    pub fn delete_entry(&mut self, favorite_id: &str) -> Result<(), String> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.id == favorite_id)
            .ok_or("收藏记录不存在，请刷新后重试")?;
        self.entries.remove(index);
        Ok(())
    }

    /// A result-row star represents this IP across the entire library.
    pub fn delete_ip(&mut self, ip: &str) -> Result<(), String> {
        let ip = ip
            .parse::<Ipv4Addr>()
            .map_err(|_| "请输入有效的 IPv4 地址".to_string())?
            .to_string();
        self.entries.retain(|entry| entry.result.ip != ip);
        Ok(())
    }

    /// Resolve exact favorite IDs before launching. No discovery sampling or
    /// per-country shortlist is involved in a favorites test.
    pub fn prepare_test(
        &mut self,
        mut run: ScanRun,
        folder_id: Option<&str>,
        favorite_id: Option<&str>,
        ip: Option<&str>,
        note: Option<&str>,
    ) -> Result<(FavoriteTestJob, Vec<Ipv4Addr>), String> {
        let mut manual_target = None;
        let favorite_ids = match (folder_id, favorite_id, ip) {
            (Some(folder_id), None, Some(ip)) => {
                self.folder_index(folder_id)?;
                let ip = checked_ipv4(ip)?;
                let note = note.map(checked_note).transpose()?;
                let existing = self.entries.iter().position(|entry| {
                    entry.folder_id == folder_id && entry.result.ip == ip.to_string()
                });
                if let Some(index) = existing {
                    if let Some(note) = note {
                        self.entries[index].note = note;
                    }
                    vec![self.entries[index].id.clone()]
                } else {
                    manual_target = Some((folder_id.to_string(), ip, note.unwrap_or_default()));
                    Vec::new()
                }
            }
            (Some(folder_id), None, None) if note.is_none() => {
                self.folder_index(folder_id)?;
                self.entries
                    .iter()
                    .filter(|entry| entry.folder_id == folder_id)
                    .map(|entry| entry.id.clone())
                    .collect()
            }
            (None, Some(favorite_id), None) if note.is_none() => {
                if !self.entries.iter().any(|entry| entry.id == favorite_id) {
                    return Err("收藏记录不存在，请刷新后重试".into());
                }
                vec![favorite_id.to_string()]
            }
            _ => {
                return Err("请选择一个收藏记录、一个收藏夹，或在收藏夹中输入一个 IPv4 地址".into())
            }
        };
        let ips = if let Some((_, ip, _)) = &manual_target {
            vec![*ip]
        } else {
            favorite_ids
                .iter()
                .map(|id| {
                    let entry = self.entries.iter().find(|entry| &entry.id == id).unwrap();
                    checked_ipv4(&entry.result.ip)
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        if ips.is_empty() {
            return Err("这个收藏夹还没有 IP，请先手动添加或收藏测试结果".into());
        }
        if ips.len() > 10_000 {
            return Err("一次最多测试 10000 个收藏 IP，请拆分收藏夹后重试".into());
        }
        let mut seen = HashSet::new();
        if ips.iter().any(|ip| !seen.insert(*ip)) {
            return Err("收藏夹中存在重复 IP，请删除重复记录后重试".into());
        }
        run.config.source = "custom".into();
        run.config.custom_ips = ips
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        run.config.validate()?;
        run.results = ips.iter().copied().map(IpResult::new).collect();
        run.progress.total = ips.len();
        run.progress.message = format!("准备测试 {} 个收藏 IP", ips.len());
        let mut favorite_ids = favorite_ids;
        if let Some((folder_id, ip, note)) = manual_target {
            self.save_snapshot(&folder_id, &run, &ip.to_string(), &note)?;
            self.entries.last_mut().unwrap().targeted = true;
            favorite_ids.push(self.entries.last().unwrap().id.clone());
        }
        Ok((FavoriteTestJob { run, favorite_ids }, ips))
    }

    /// Match by captured ID so edits/moves survive, deleted records stay
    /// deleted, and a same-IP record in another folder is never overwritten.
    pub fn apply_test_results(&mut self, job: &FavoriteTestJob) {
        for favorite_id in &job.favorite_ids {
            let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| &entry.id == favorite_id)
            else {
                continue;
            };
            let Some(result) = job
                .run
                .results
                .iter()
                .find(|result| result.ip == entry.result.ip)
            else {
                continue;
            };
            let is_new_placeholder = entry.run_id == job.run.id && entry.run_status == "running";
            if job.run.status != "completed" && !was_attempted(result) && !is_new_placeholder {
                continue;
            }
            entry.run_id = job.run.id.clone();
            entry.run_status = job.run.status.clone();
            entry.run_started_at = job.run.started_at.clone();
            entry.run_finished_at = job.run.finished_at.clone();
            entry.config = job.run.config.clone();
            entry.result = result.clone();
            entry.targeted = true;
            entry.network_path = job.run.progress.network_path.clone();
        }
    }
}

fn checked_ipv4(ip: &str) -> Result<Ipv4Addr, String> {
    ip.trim().parse::<Ipv4Addr>().map_err(|_| {
        "请输入一个有效的 IPv4 地址，例如 104.16.1.1（不支持域名、CIDR 或 IPv6）".into()
    })
}

fn was_attempted(result: &IpResult) -> bool {
    result.tcp_ms > 0.0
        || result.attempts > 0
        || result
            .tcp_stability
            .as_ref()
            .is_some_and(|stats| !stats.probes_ms.is_empty())
        || result
            .exclusion_reason
            .as_ref()
            .is_some_and(|reason| reason != "测试已停止，验证或下载复测未完成")
}

fn checked_note(note: &str) -> Result<String, String> {
    let note = note.trim();
    if note.chars().count() > 500 {
        return Err("收藏备注不能超过 500 个字符".into());
    }
    Ok(note.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn sample_run() -> ScanRun {
        let mut result = IpResult::new("104.16.1.1".parse().unwrap());
        result.download_samples_mbps = vec![30.0, 40.0];
        result.download_mbps = 35.0;
        result.tcp_stability = Some(crate::stability::summarize(
            100,
            230.0,
            vec![Some(20.0); 100],
        ));
        ScanRun {
            id: "original-run".into(),
            status: "completed".into(),
            started_at: "2026-10-23T00:00:00Z".into(),
            finished_at: Some("2026-10-23T00:01:00Z".into()),
            config: ScanConfig::default(),
            progress: crate::model::ScanProgress {
                network_path: Some(NetworkPath {
                    sample_ip: result.ip.clone(),
                    interface_name: "en0".into(),
                    tunnel_interface: false,
                    captured_at: "2026-10-23T00:00:01Z".into(),
                }),
                ..Default::default()
            },
            results: vec![result],
            error: None,
        }
    }

    #[test]
    fn library_revision_is_legacy_compatible_and_stays_exact_in_javascript() {
        let mut json = serde_json::to_value(FavoriteLibrary::default()).unwrap();
        assert_eq!(json["revision"], 0);
        json.as_object_mut().unwrap().remove("revision");
        let mut legacy: FavoriteLibrary = serde_json::from_value(json).unwrap();
        assert_eq!(legacy.revision, 0);
        assert_eq!(legacy.next_revision().unwrap(), 1);
        // Revisions are allocated by the atomic saved-state commit, not by
        // domain edits that might still be rejected or fail to persist.
        legacy.create_folder("候选编辑").unwrap();
        assert_eq!(legacy.revision, 0);
        legacy.revision = 9_007_199_254_740_990;
        assert_eq!(legacy.next_revision().unwrap(), 9_007_199_254_740_991);
        legacy.revision += 1;
        assert!(legacy.next_revision().is_err());
        assert_eq!(legacy.revision, 9_007_199_254_740_991);
        legacy.revision = u64::MAX;
        assert!(legacy.next_revision().is_err());
    }

    #[test]
    fn folder_names_are_trimmed_unique_and_bounded_without_recreating_deleted_defaults() {
        let mut favorites = FavoriteLibrary::default();
        assert_eq!(
            favorites
                .folders
                .iter()
                .map(|folder| folder.name.as_str())
                .collect::<Vec<_>>(),
            ["联通流量", "联通宽带", "移动流量"]
        );
        favorites.create_folder("  公司宽带  ").unwrap();
        assert_eq!(favorites.folders[3].name, "公司宽带");
        assert!(favorites.create_folder(" 公司宽带 ").is_err());
        assert!(favorites.create_folder(" ").is_err());
        assert!(favorites.create_folder(&"中".repeat(41)).is_err());
        let id = favorites.folders[3].id.clone();
        favorites.rename_folder(&id, " 公司宽带 ").unwrap();
        assert!(favorites.rename_folder(&id, "联通流量").is_err());
        while let Some(folder) = favorites.folders.first() {
            let id = folder.id.clone();
            favorites.delete_folder(&id).unwrap();
        }
        assert!(favorites.folders.is_empty());
    }

    #[test]
    fn saving_again_updates_only_that_folder_snapshot_and_preserves_entry_id() {
        let mut favorites = FavoriteLibrary::default();
        let first = favorites.folders[0].id.clone();
        let second = favorites.folders[1].id.clone();
        let mut run = sample_run();
        let ip = run.results[0].ip.clone();
        favorites
            .save_snapshot(&first, &run, &ip, " 原记录 ")
            .unwrap();
        let first_id = favorites.entries[0].id.clone();
        favorites.save_snapshot(&second, &run, &ip, "宽带").unwrap();
        run.id = "next-run".into();
        run.results[0].download_mbps = 50.0;
        favorites
            .save_snapshot(&first, &run, &ip, "新记录")
            .unwrap();
        assert_eq!(favorites.entries.len(), 2);
        assert_eq!(favorites.entries[0].id, first_id);
        assert_eq!(favorites.entries[0].run_id, "next-run");
        assert_eq!(favorites.entries[0].result.download_mbps, 50.0);
        assert_eq!(favorites.entries[0].note, "新记录");
        assert_eq!(favorites.entries[1].run_id, "original-run");
        assert_eq!(favorites.entries[1].result.download_mbps, 35.0);
    }

    #[test]
    fn unstar_removes_the_ip_from_every_folder_and_preserves_other_entries() {
        let mut favorites = FavoriteLibrary::default();
        let first = favorites.folders[0].id.clone();
        let second = favorites.folders[1].id.clone();
        let mut run = sample_run();
        let ip = run.results[0].ip.clone();
        favorites.save_snapshot(&first, &run, &ip, "流量").unwrap();
        favorites.save_snapshot(&second, &run, &ip, "宽带").unwrap();
        run.results[0].ip = "192.0.2.42".into();
        favorites
            .save_snapshot(&first, &run, "192.0.2.42", "保留")
            .unwrap();
        let remaining = serde_json::to_value(&favorites.entries[2]).unwrap();
        assert!(favorites.delete_ip("invalid").is_err());
        assert_eq!(favorites.entries.len(), 3);
        favorites.delete_ip(&ip).unwrap();
        assert_eq!(favorites.entries.len(), 1);
        assert_eq!(
            serde_json::to_value(&favorites.entries[0]).unwrap(),
            remaining
        );
        assert_eq!(favorites.folders.len(), 3);
        // Repeated clicks or a delayed snapshot may already have removed it.
        favorites.delete_ip(&ip).unwrap();
        assert_eq!(favorites.entries.len(), 1);
    }

    #[test]
    fn move_rejects_duplicate_destination_and_edits_keep_measurements_unchanged() {
        let mut favorites = FavoriteLibrary::default();
        let first = favorites.folders[0].id.clone();
        let second = favorites.folders[1].id.clone();
        let third = favorites.folders[2].id.clone();
        let run = sample_run();
        let ip = &run.results[0].ip;
        favorites.save_snapshot(&first, &run, ip, "before").unwrap();
        favorites.save_snapshot(&second, &run, ip, "other").unwrap();
        let id = favorites.entries[0].id.clone();
        let original = serde_json::to_value(&favorites.entries[0]).unwrap();
        assert!(favorites.update_entry(&id, &second, "rejected").is_err());
        assert_eq!(
            serde_json::to_value(&favorites.entries[0]).unwrap(),
            original
        );
        assert!(favorites
            .update_entry(&id, &third, &"中".repeat(501))
            .is_err());
        favorites.update_entry(&id, &third, " after ").unwrap();
        let entry = &favorites.entries[0];
        assert_eq!(entry.folder_id, third);
        assert_eq!(entry.note, "after");
        assert_eq!(
            serde_json::to_value(&entry.result).unwrap(),
            original["result"]
        );
        assert_eq!(
            serde_json::to_value(&entry.config).unwrap(),
            original["config"]
        );
        assert_eq!(entry.saved_at, original["savedAt"].as_str().unwrap());
        favorites.delete_folder(&third).unwrap();
        assert_eq!(favorites.entries.len(), 1);
        let remaining = favorites.entries[0].id.clone();
        favorites.delete_entry(&remaining).unwrap();
        assert!(favorites.entries.is_empty());
    }

    #[test]
    fn snapshot_is_detached_from_run_and_carries_partial_test_context() {
        let mut favorites = FavoriteLibrary::default();
        let folder = favorites.folders[0].id.clone();
        let mut run = sample_run();
        run.status = "running".into();
        run.finished_at = None;
        let ip = run.results[0].ip.clone();
        favorites
            .save_snapshot(&folder, &run, &ip, "移动网络")
            .unwrap();
        let original = serde_json::to_value(&favorites.entries[0]).unwrap();
        run.config.max_tcp_ms = Some(100.0);
        run.results[0].download_samples_mbps.clear();
        run.results[0]
            .tcp_stability
            .as_mut()
            .unwrap()
            .probes_ms
            .clear();
        run.progress.network_path.as_mut().unwrap().interface_name = "utun8".into();
        run.status = "completed".into();
        assert_eq!(
            serde_json::to_value(&favorites.entries[0]).unwrap(),
            original
        );
        assert_eq!(favorites.entries[0].run_status, "running");
        assert!(favorites.entries[0].run_finished_at.is_none());
        assert_eq!(
            favorites.entries[0]
                .network_path
                .as_ref()
                .unwrap()
                .interface_name,
            "en0"
        );
        assert!(favorites
            .save_snapshot(&folder, &run, "missing", "")
            .is_err());
        assert!(favorites.save_snapshot("missing", &run, &ip, "").is_err());
    }

    fn pending_run() -> ScanRun {
        let mut run = sample_run();
        run.id = "favorite-test".into();
        run.status = "running".into();
        run.finished_at = None;
        run.results.clear();
        run
    }

    #[test]
    fn manual_test_requires_one_strict_ipv4_and_unambiguous_target() {
        let mut favorites = FavoriteLibrary::default();
        let folder = favorites.folders[0].id.clone();
        for invalid in [
            "",
            "example.com",
            "104.16.1.1/32",
            "::1",
            "104.16.1.1\n104.16.1.2",
            "010.0.0.1",
            "256.1.1.1",
        ] {
            assert!(
                favorites
                    .prepare_test(pending_run(), Some(&folder), None, Some(invalid), None)
                    .is_err(),
                "accepted {invalid}"
            );
        }
        assert!(favorites
            .prepare_test(pending_run(), None, None, Some("104.16.1.1"), None)
            .is_err());
        assert!(favorites
            .prepare_test(
                pending_run(),
                Some(&folder),
                Some("missing"),
                Some("104.16.1.1"),
                None
            )
            .is_err());
        assert!(favorites
            .prepare_test(pending_run(), Some(&folder), None, None, None)
            .is_err());
        let (job, ips) = favorites
            .prepare_test(
                pending_run(),
                Some(&folder),
                None,
                Some(" 104.16.1.1 "),
                Some(" 手动添加 "),
            )
            .unwrap();
        assert_eq!(ips, vec!["104.16.1.1".parse::<Ipv4Addr>().unwrap()]);
        assert_eq!(job.favorite_ids.len(), 1);
        assert_eq!(favorites.entries[0].note, "手动添加");
        assert!(favorites.entries[0].targeted);
        assert_eq!(favorites.entries[0].run_status, "running");
        assert_eq!(favorites.entries[0].result.attempts, 0);
        assert!(favorites.entries[0].result.tcp_stability.is_none());
        assert_eq!(job.run.config.custom_ips, "104.16.1.1");
    }

    #[test]
    fn folder_test_includes_every_entry_despite_discovery_and_regional_caps() {
        let mut favorites = FavoriteLibrary::default();
        let folder = favorites.folders[0].id.clone();
        let mut run = sample_run();
        for host in 1..=25 {
            let ip = format!("104.16.1.{host}");
            run.results[0].ip = ip.clone();
            favorites.save_snapshot(&folder, &run, &ip, "").unwrap();
        }
        let mut pending = pending_run();
        pending.config.max_candidates = 1;
        pending.config.regional_candidate_limit = Some(3);
        pending.config.source = "custom".into();
        pending.config.custom_ips.clear();
        let (job, ips) = favorites
            .prepare_test(pending, Some(&folder), None, None, None)
            .unwrap();
        assert_eq!(ips.len(), 25);
        assert_eq!(job.favorite_ids.len(), 25);
        assert_eq!(job.run.results.len(), 25);
        assert!(job
            .run
            .results
            .iter()
            .all(|result| result.attempts == 0 && result.download_samples_mbps.is_empty()));
        assert_eq!(job.run.config.max_candidates, 1);
        assert_eq!(favorites.entries[0].run_id, "original-run");
    }

    #[test]
    fn manual_existing_ip_reuses_its_id_without_discarding_the_saved_measurement() {
        let mut favorites = FavoriteLibrary::default();
        let folder = favorites.folders[0].id.clone();
        let run = sample_run();
        favorites
            .save_snapshot(&folder, &run, "104.16.1.1", "keep")
            .unwrap();
        let original = serde_json::to_value(&favorites.entries[0]).unwrap();
        let (job, _) = favorites
            .prepare_test(pending_run(), Some(&folder), None, Some("104.16.1.1"), None)
            .unwrap();
        assert_eq!(favorites.entries.len(), 1);
        assert_eq!(job.favorite_ids[0], favorites.entries[0].id);
        assert_eq!(
            serde_json::to_value(&favorites.entries[0]).unwrap(),
            original
        );
    }

    #[test]
    fn folder_test_rejects_duplicate_or_excessive_targets_instead_of_truncating() {
        let mut favorites = FavoriteLibrary::default();
        let folder = favorites.folders[0].id.clone();
        favorites
            .save_snapshot(&folder, &sample_run(), "104.16.1.1", "")
            .unwrap();
        let original = favorites.entries[0].clone();
        let mut duplicate = original.clone();
        duplicate.id = "duplicate".into();
        favorites.entries.push(duplicate);
        assert!(favorites
            .prepare_test(pending_run(), Some(&folder), None, None, None)
            .unwrap_err()
            .contains("重复"));
        favorites.entries = (0..10_001)
            .map(|index| {
                let mut entry = original.clone();
                entry.id = index.to_string();
                entry.result.ip =
                    Ipv4Addr::new(10, 0, (index / 256) as u8, (index % 256) as u8).to_string();
                entry
            })
            .collect();
        assert!(favorites
            .prepare_test(pending_run(), Some(&folder), None, None, None)
            .unwrap_err()
            .contains("10000"));
    }

    #[test]
    fn completed_retest_matches_ids_and_preserves_edits_dates_and_other_folders() {
        let mut favorites = FavoriteLibrary::default();
        let first = favorites.folders[0].id.clone();
        let second = favorites.folders[1].id.clone();
        let third = favorites.folders[2].id.clone();
        let run = sample_run();
        let ip = run.results[0].ip.clone();
        favorites.save_snapshot(&first, &run, &ip, "old").unwrap();
        favorites
            .save_snapshot(&second, &run, &ip, "other")
            .unwrap();
        let id = favorites.entries[0].id.clone();
        let saved_at = favorites.entries[0].saved_at.clone();
        let other = serde_json::to_value(&favorites.entries[1]).unwrap();
        let (mut job, _) = favorites
            .prepare_test(pending_run(), None, Some(&id), None, None)
            .unwrap();
        favorites
            .update_entry(&id, &third, "edited during test")
            .unwrap();
        job.run.status = "completed".into();
        job.run.results[0].download_mbps = 99.0;
        favorites.apply_test_results(&job);
        let entry = &favorites.entries[0];
        assert_eq!(entry.id, id);
        assert_eq!(entry.folder_id, third);
        assert_eq!(entry.note, "edited during test");
        assert_eq!(entry.saved_at, saved_at);
        assert_eq!(entry.result.download_mbps, 99.0);
        assert!(entry.targeted);
        assert_eq!(serde_json::to_value(&favorites.entries[1]).unwrap(), other);
        favorites.delete_entry(&id).unwrap();
        favorites.apply_test_results(&job);
        assert_eq!(favorites.entries.len(), 1);
    }

    #[test]
    fn cancelled_retest_retains_unattempted_measurements_but_saves_failures_and_manual_placeholder()
    {
        let mut favorites = FavoriteLibrary::default();
        let folder = favorites.folders[0].id.clone();
        let mut run = sample_run();
        favorites
            .save_snapshot(&folder, &run, "104.16.1.1", "keep")
            .unwrap();
        run.results[0].ip = "104.16.1.2".into();
        favorites
            .save_snapshot(&folder, &run, "104.16.1.2", "failed")
            .unwrap();
        let original = serde_json::to_value(&favorites.entries[0]).unwrap();
        let (mut job, _) = favorites
            .prepare_test(pending_run(), Some(&folder), None, None, None)
            .unwrap();
        job.run.results[0].exclusion_reason = Some("测试已停止，验证或下载复测未完成".into());
        job.run.results[1].exclusion_reason = Some("TCP 连接超时".into());
        job.interrupt("cancelled");
        favorites.apply_test_results(&job);
        assert_eq!(
            serde_json::to_value(&favorites.entries[0]).unwrap(),
            original
        );
        assert_eq!(favorites.entries[1].run_status, "cancelled");
        assert_eq!(
            favorites.entries[1].result.exclusion_reason.as_deref(),
            Some("TCP 连接超时")
        );
        let (mut manual, _) = favorites
            .prepare_test(pending_run(), Some(&folder), None, Some("104.16.1.3"), None)
            .unwrap();
        manual.interrupt("cancelled");
        favorites.apply_test_results(&manual);
        assert_eq!(favorites.entries[2].run_status, "cancelled");
        assert_eq!(favorites.entries[2].result.attempts, 0);
    }
}

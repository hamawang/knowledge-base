//! V1 拉取：远端 → 本地
//!
//! 流程：
//! 1. 读远端 manifest（首次 → 当无操作返回）
//! 2. 计算本地 manifest
//! 3. diff
//! 4. 对 to_pull：从 backend.get_note 拉 .md 文本 → 解析 title + body → upsert 到本地
//! 5. 对 to_delete_local：软删本地笔记（v1 不实际删，仅 set is_deleted=1）
//! 6. 冲突 (conflicts)：默认 last-write-wins（按 updated_at 较新者赢）。两种情况会把远端版本
//!    落到 `<app_data>/sync_conflicts/backend_<id>/<sid>_<ts>.md`，本地保持原样、等用户在设置页合并：
//!      a) 双方 updated_at 完全相同但内容 hash 不同（manifest diff 的 `conflicts` 集合，极小概率）
//!      b) T-S051：本地有未推送改动 + 远端也改了（to_pull 里 `is_divergence` 检测命中）

use std::path::Path;

use tauri::{Emitter, Runtime};

use crate::database::Database;
use crate::error::AppError;
use crate::models::{NoteInput, SyncManifestV1, SyncPullResult};

use super::backend::SyncBackendImpl;
use super::manifest;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressEvent {
    backend_id: i64,
    phase: String, // "compute" | "diff" | "download" | "apply" | "done"
    current: usize,
    total: usize,
    message: String,
}

pub fn pull<R: Runtime, E: Emitter<R>>(
    db: &Database,
    backend_id: i64,
    backend: &dyn SyncBackendImpl,
    app_version: &str,
    device: &str,
    conflicts_dir: &Path,
    data_dir: &Path,
    emitter: &E,
) -> Result<SyncPullResult, AppError> {
    let mut result = SyncPullResult::default();
    let event_name = "sync_v1:progress";

    let _ = emitter.emit(
        event_name,
        ProgressEvent {
            backend_id,
            phase: "compute".into(),
            current: 0,
            total: 0,
            message: "拉取远端 manifest…".into(),
        },
    );
    let remote = match backend.read_manifest()? {
        Some(m) => m,
        None => {
            // 远端没东西，无操作
            return Ok(result);
        }
    };

    // hash 算法兼容性检查（v1 → v2 升级）：
    // 远端 manifest 不带 hash_algo（旧客户端写的）且有内容 → 当前的 v2 公式与远端不一致，
    // diff 会把所有笔记误判为变更。处理：清空本机 sync_remote_state（防止误判跳过），
    // 本次 pull 直接退出；下次 push 会把本地全部笔记当作首次推送，写出 v2 格式 manifest 完成升级。
    if !remote.entries.is_empty()
        && remote.hash_algo.as_deref() != Some(SyncManifestV1::HASH_ALGO_V2)
    {
        log::warn!(
            "[sync_v1] backend {} 远端 manifest 用旧 hash 算法 ({:?})，跳过本次 pull 并清空本地 sync_remote_state；下次 push 将全量重传升级到 v2",
            backend_id,
            remote.hash_algo
        );
        let cleared = db.clear_remote_state_for_backend(backend_id)?;
        log::info!("[sync_v1] 已清空 {} 条 sync_remote_state（backend {}）", cleared, backend_id);
        return Ok(result);
    }

    // T-S014：vault meta 跨端同步
    // - 远端有 + 本机无 → 导入（用户用相同密码可解锁）
    // - 远端有 + 本机有 + salt 不同 → 警告（加密笔记会跳过同步，普通笔记照常）
    // - 远端有 + 本机有 + salt 相同 → 一致，加密笔记可互通
    // - 远端无 → 不处理
    let remote_vault_compatible = match remote.vault.as_ref() {
        None => false, // 远端没设置 vault → 加密笔记无法跨端
        Some(meta) => {
            match crate::services::vault::VaultService::import_meta_if_not_set(db, meta) {
                Ok(true) => {
                    log::info!(
                        "[sync_v1] 本机 vault 从远端 manifest 导入 salt+verifier（首次同步加密笔记）"
                    );
                    true
                }
                Ok(false) => {
                    // 本机已有，比对 salt
                    match crate::services::vault::VaultService::meta_matches(db, meta) {
                        Ok(true) => true,
                        Ok(false) => {
                            log::warn!(
                                "[sync_v1] 远端 vault salt 与本机不同，加密笔记不参与本次同步"
                            );
                            false
                        }
                        Err(e) => {
                            log::warn!("[sync_v1] vault meta 比对失败 {}: 加密笔记跳过", e);
                            false
                        }
                    }
                }
                Err(e) => {
                    log::warn!("[sync_v1] 导入远端 vault meta 失败: {}（加密笔记跳过）", e);
                    false
                }
            }
        }
    };

    let local = manifest::compute_local_manifest(db, app_version, device)?;

    let _ = emitter.emit(
        event_name,
        ProgressEvent {
            backend_id,
            phase: "diff".into(),
            current: 0,
            total: 0,
            message: "对比本地…".into(),
        },
    );
    let diff = manifest::diff_manifests(&local, &remote);

    // ── T-S024：附件下载阶段（先于笔记 entry pull，让笔记内容拉下来时附件已就位）
    //
    // 流程：
    //   远端 manifest.attachments - 本地 unique hashes → 差集要下载
    //   下载内容写到 {prefix}kb_assets/sync_in/<hash>.<ext>（dev/prod 目录前缀对齐）
    //
    // 路径还原说明：pull 端的笔记 .md 里可能引用 `kb_assets/images/xxx.png` 等原始路径，
    // 但拉到的实际文件落在 sync_in/ → 编辑器/渲染器需要做 hash 反查 fallback。
    // 这是后续 UI 任务（不在 T-S024 范围），现阶段保证字节到达本地即可。
    let local_hashes: std::collections::HashSet<String> = db
        .list_all_unique_attachments()
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.sha256_hex)
        .collect();
    let to_download: Vec<&crate::models::AttachmentEntry> = remote
        .attachments
        .iter()
        .filter(|a| !local_hashes.contains(&a.hash))
        .collect();
    let total_dl = to_download.len();
    if total_dl > 0 {
        let assets_prefix = if cfg!(debug_assertions) {
            "dev-kb_assets"
        } else {
            "kb_assets"
        };
        let sync_in_dir = data_dir.join(assets_prefix).join("sync_in");
        if let Err(e) = std::fs::create_dir_all(&sync_in_dir) {
            log::warn!("[sync_v1] 创建 sync_in 目录失败 ({}): {}", sync_in_dir.display(), e);
        }

        for (idx, att) in to_download.iter().enumerate() {
            let _ = emitter.emit(
                event_name,
                ProgressEvent {
                    backend_id,
                    phase: "attachments".into(),
                    current: idx + 1,
                    total: total_dl,
                    message: format!(
                        "下载附件 {} ({} bytes)",
                        &att.hash[..att.hash.len().min(8)],
                        att.size
                    ),
                },
            );

            match backend.get_attachment(&att.hash) {
                Ok(Some(bytes)) => {
                    let ext = att.ext.as_deref().unwrap_or("bin");
                    let target = sync_in_dir.join(format!("{}.{}", att.hash, ext));
                    match std::fs::write(&target, &bytes) {
                        Ok(_) => result.attachments_downloaded += 1,
                        Err(e) => result.errors.push(format!(
                            "写入附件 {} 失败: {}",
                            target.display(),
                            e
                        )),
                    }
                }
                Ok(None) => result.errors.push(format!(
                    "远端 manifest 有附件 {} 但 get_attachment 返回空",
                    &att.hash[..att.hash.len().min(8)]
                )),
                Err(e) => result.errors.push(format!(
                    "下载附件 {} 失败: {}",
                    &att.hash[..att.hash.len().min(8)],
                    e
                )),
            }
        }
    }

    // T-S051: 分歧检测准备数据
    //  - local_hash_by_uuid：本地每条笔记的当前内容 hash（用 stable_uuid 索引）
    //  - remote_states：本地与该 backend 的同步状态（含 last_synced_hash = 上次同步时的内容 hash）
    // 若一条笔记在 to_pull 里（远端较新），但本地当前 hash 已偏离 last_synced（说明本地也改过了），
    // 且与远端 hash 也不同 → 双方各改各的 → 不静默覆盖本地，而是把远端版本落冲突文件等用户合并。
    let local_hash_by_uuid: std::collections::HashMap<&str, &str> = local
        .entries
        .iter()
        .map(|e| (e.stable_id.as_str(), e.content_hash.as_str()))
        .collect();
    let remote_states = db.list_remote_state(backend_id)?;

    // ── 处理 to_pull（远端独有 / 远端较新）
    let total_pull = diff.to_pull.len();
    for (idx, entry) in diff.to_pull.iter().enumerate() {
        let _ = emitter.emit(
            event_name,
            ProgressEvent {
                backend_id,
                phase: "download".into(),
                current: idx + 1,
                total: total_pull,
                message: format!("下载 {}", entry.title),
            },
        );
        let body = match backend.get_note(&entry.remote_path)? {
            Some(s) => s,
            None => {
                result.errors.push(format!(
                    "远端 manifest 有 {} 但 .md 文件丢失",
                    entry.remote_path
                ));
                continue;
            }
        };
        let folder_id = ensure_folder_path(db, &entry.folder_path)?;

        // T-S014：加密笔记走密文 upsert 分支
        if entry.encrypted {
            if !remote_vault_compatible {
                log::warn!(
                    "[sync_v1] 跳过加密笔记 {}（vault meta 不匹配或缺失）",
                    entry.title
                );
                continue;
            }
            use base64::Engine as _;
            let blob = match base64::engine::general_purpose::STANDARD.decode(body.as_bytes()) {
                Ok(b) => b,
                Err(e) => {
                    result.errors.push(format!(
                        "加密笔记 {} base64 解码失败: {}",
                        entry.title, e
                    ));
                    continue;
                }
            };
            match db.upsert_encrypted_note_with_uuid(
                &entry.stable_id,
                &entry.title,
                &blob,
                folder_id,
            ) {
                Ok(local_id) => {
                    result.downloaded += 1;
                    if let Err(e) = db.upsert_remote_state(
                        backend_id,
                        local_id,
                        &entry.remote_path,
                        &entry.content_hash,
                        &entry.updated_at,
                        false,
                    ) {
                        result
                            .errors
                            .push(format!("upsert sync_remote_state 失败: {}", e));
                    }
                }
                Err(e) => result
                    .errors
                    .push(format!("写入加密笔记失败 {}: {}", entry.title, e)),
            }
            continue;
        }

        // 非加密笔记：原 markdown 路径
        let (title, content) = parse_note_md(&body, &entry.title);
        let input = NoteInput {
            title,
            content,
            folder_id,
        };

        // T-S011：entry.stable_id 现在是 UUID（v36）。先按 stable_uuid 查本地 → 决定 update/create
        let local_id_for_state = match db.get_note_id_by_stable_uuid(&entry.stable_id)? {
            Some(local_id) => {
                // T-S051: 分歧检测 —— 本地有未推送改动且与远端各不相同 → 不覆盖，落冲突文件保留本地
                let diverged = is_divergence(
                    local_hash_by_uuid.get(entry.stable_id.as_str()).copied(),
                    remote_states.get(&local_id).map(|s| s.last_synced_hash.as_str()),
                    &entry.content_hash,
                );
                if diverged {
                    if let Err(e) = std::fs::create_dir_all(conflicts_dir) {
                        result
                            .errors
                            .push(format!("创建冲突目录失败 {}: {}", conflicts_dir.display(), e));
                    }
                    let safe_id = entry.stable_id.replace('/', "_");
                    let path = conflicts_dir.join(format!(
                        "{}_{}.md",
                        safe_id,
                        entry.updated_at.replace([':', ' '], "-")
                    ));
                    match std::fs::write(&path, &body) {
                        Ok(_) => {
                            result.conflicts += 1;
                            log::warn!(
                                "[sync_v1] 笔记 {} 本地/远端各改各的，已把远端版本落冲突文件 {}，本地保留",
                                entry.title,
                                path.display()
                            );
                        }
                        Err(e) => result.errors.push(format!("写冲突文件失败: {}", e)),
                    }
                    continue; // 不 update_note、不 upsert_remote_state → 保留本地，等用户在设置页解决
                }
                match db.update_note(local_id, &input) {
                    Ok(_) => {
                        // 修复日记重复 bug：把"每日笔记"标记对齐到远端 manifest entry
                        // （远端是日记本地却不是 → 恢复 is_daily/daily_date；反之则清掉标记）
                        if let Err(e) = db.sync_note_daily_state(
                            local_id,
                            entry.is_daily,
                            entry.daily_date.as_deref(),
                        ) {
                            result
                                .errors
                                .push(format!("对齐日记标记失败 {}: {}", entry.title, e));
                        }
                        Some(local_id)
                    }
                    Err(e) => {
                        result
                            .errors
                            .push(format!("更新本地笔记失败 {}: {}", entry.title, e));
                        None
                    }
                }
            }
            None => {
                // 本地没有 → 用远端 UUID 创建（保持多端 ID 稳定）+ 透传 is_daily/daily_date
                // （否则拉来的日记会变成普通笔记，对端 get_or_create_daily 认不出来 → 反复新建）
                match db.create_note_with_uuid(
                    &input,
                    &entry.stable_id,
                    entry.is_daily,
                    entry.daily_date.as_deref(),
                ) {
                    Ok(n) => Some(n.id),
                    Err(e) => {
                        result
                            .errors
                            .push(format!("新建本地笔记失败 {}: {}", entry.title, e));
                        None
                    }
                }
            }
        };

        if let Some(local_id) = local_id_for_state {
            result.downloaded += 1;
            if let Err(e) = db.upsert_remote_state(
                backend_id,
                local_id,
                &entry.remote_path,
                &entry.content_hash,
                &entry.updated_at,
                false,
            ) {
                result
                    .errors
                    .push(format!("upsert sync_remote_state 失败: {}", e));
            }
        }
    }

    // ── 处理 to_delete_local（远端 tombstone）
    for entry in &diff.to_delete_local {
        // T-S011：按 stable_uuid 找本地 id，没找到说明本地本就没有此笔记，跳过
        let local_id = match db.get_note_id_by_stable_uuid(&entry.stable_id)? {
            Some(id) => id,
            None => continue,
        };
        match db.soft_delete_note(local_id) {
            Ok(true) => {
                result.deleted_local += 1;
                let _ = db.upsert_remote_state(
                    backend_id,
                    local_id,
                    &entry.remote_path,
                    &entry.content_hash,
                    &entry.updated_at,
                    true,
                );
            }
            Ok(false) => {} // 本地已没有
            Err(e) => result
                .errors
                .push(format!("软删本地失败 {}: {}", entry.title, e)),
        }
    }

    // ── 处理 conflicts（updated_at 相同但 hash 不同）
    if !diff.conflicts.is_empty() {
        std::fs::create_dir_all(conflicts_dir).ok();
    }
    for pair in &diff.conflicts {
        result.conflicts += 1;
        // 把远端版本落地到 .conflicts/，让用户手动选
        match backend.get_note(&pair.remote.remote_path) {
            Ok(Some(remote_body)) => {
                let safe_id = pair.remote.stable_id.replace('/', "_");
                let path = conflicts_dir.join(format!(
                    "{}_{}.md",
                    safe_id,
                    pair.remote.updated_at.replace([':', ' '], "-")
                ));
                if let Err(e) = std::fs::write(&path, remote_body) {
                    result.errors.push(format!("写冲突文件失败: {}", e));
                }
            }
            Ok(None) => {}
            Err(e) => result.errors.push(format!("拉远端冲突文件失败: {}", e)),
        }
    }

    db.touch_sync_backend_pull(backend_id)?;

    let _ = emitter.emit(
        event_name,
        ProgressEvent {
            backend_id,
            phase: "done".into(),
            current: 0,
            total: 0,
            message: format!(
                "拉取完成: 下载 {} / 删本地 {} / 冲突 {} / 错误 {}",
                result.downloaded,
                result.deleted_local,
                result.conflicts,
                result.errors.len()
            ),
        },
    );

    Ok(result)
}

/// T-S051: 判定一条 to_pull 笔记是否"本地远端各改各的"
///
/// 条件：本地当前 hash 已知 + 上次同步 hash 已知 + 本地 ≠ 上次同步（本地有未推送改动）
///       + 本地 ≠ 远端（远端确实带来了不同内容）→ 真分歧。
/// 任一信息缺失（如该笔记从未同步过、本地刚 create）→ 不算分歧（按原 last-write-wins 走）。
fn is_divergence(local_hash: Option<&str>, last_synced_hash: Option<&str>, remote_hash: &str) -> bool {
    match (local_hash, last_synced_hash) {
        (Some(lh), Some(ls)) => lh != ls && lh != remote_hash,
        _ => false,
    }
}

/// 解析 .md 文件：第一个 `# ` 行作为 title，其余作为 content
///
/// 如解析不到 # 标题，回退到 manifest entry 里的 title + 全文作为 content
fn parse_note_md(body: &str, fallback_title: &str) -> (String, String) {
    let mut lines = body.lines();
    let first = lines.next().unwrap_or("").trim();
    if let Some(rest) = first.strip_prefix("# ") {
        let title = rest.trim().to_string();
        // 跳过紧跟的空行（两个换行的写法）
        let body_rest: String = lines
            .skip_while(|l| l.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        return (title, body_rest);
    }
    (fallback_title.to_string(), body.to_string())
}

/// 把 "工作/周报" 风格的路径递归展平成 folder_id
///
/// 复用 `FolderService::ensure_path`（T-006 阶段已实现）
fn ensure_folder_path(db: &Database, path: &str) -> Result<Option<i64>, AppError> {
    if path.is_empty() {
        return Ok(None);
    }
    crate::services::folder::FolderService::ensure_path(db, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_md_with_h1() {
        let body = "# 我的标题\n\n正文内容\n第二段";
        let (t, c) = parse_note_md(body, "fallback");
        assert_eq!(t, "我的标题");
        assert_eq!(c, "正文内容\n第二段");
    }

    #[test]
    fn parse_md_no_h1_uses_fallback() {
        let body = "没有 H1 的正文";
        let (t, c) = parse_note_md(body, "manifest 标题");
        assert_eq!(t, "manifest 标题");
        assert_eq!(c, "没有 H1 的正文");
    }

    #[test]
    fn divergence_only_when_both_changed_and_differ() {
        // 本地改了（≠ 上次同步），远端也带来不同内容（≠ 本地）→ 分歧
        assert!(is_divergence(Some("localH"), Some("syncedH"), "remoteH"));
        // 本地没改（== 上次同步）→ 不是分歧，正常 last-write-wins 拉远端
        assert!(!is_divergence(Some("syncedH"), Some("syncedH"), "remoteH"));
        // 本地改了，但改成的内容恰好和远端一样 → 不算分歧（只需更新时间戳）
        assert!(!is_divergence(Some("sameH"), Some("syncedH"), "sameH"));
        // 该笔记从未同步过 / 信息缺失 → 不算分歧
        assert!(!is_divergence(Some("localH"), None, "remoteH"));
        assert!(!is_divergence(None, Some("syncedH"), "remoteH"));
    }
}

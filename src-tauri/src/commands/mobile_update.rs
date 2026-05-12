//! 移动端"检查更新"（仅 Android/iOS 编译）。
//!
//! 桌面端用 `tauri-plugin-updater` 自动下载+原地替换，但该插件不支持移动端，
//! 且本项目已用 `#[cfg(desktop)]` 把 updater 隔离掉了。移动端没有"原地热替换"
//! 的能力（Android 必须走系统安装器、iOS 必须走 App Store），所以这里只做：
//!
//!   1. 拉移动端独立的 `update-mobile.json`（**不是**桌面那份 `update.json`——
//!      移动端有自己的版本线，从 0.1.0 起，跟桌面 1.x 解耦）
//!   2. 比对 `version` 字段与当前 App 版本（Android 的版本号来自
//!      `tauri.android.conf.json` 的 `version`，会被编译进 `package_info()`）
//!   3. 返回是否有新版本 + 更新说明 + APK 下载 URL
//!
//! 前端拿到结果后弹个对话框，用户点"去下载"就用 `tauri-plugin-opener` 打开
//! APK URL —— 浏览器接管下载，下载完用户点一下，系统安装器接手（首次会引导用户
//! 开"允许安装未知应用"，那是浏览器的权限不是本 App 的，所以 manifest 不用加
//! `REQUEST_INSTALL_PACKAGES`）。
//!
//! `update-mobile.json` schema（扁平结构，只服务 Android）：
//! ```json
//! {
//!   "version": "0.1.0",
//!   "notes": "更新说明",
//!   "pub_date": "2026-...",
//!   "url": "https://.../Knowledge.Base_0.1.0_android-arm64.apk"
//! }
//! ```
//! 兼容兜底：也认旧的 `platforms.android-arm64.url` 嵌套写法；都没有则回落到
//! release 仓库的发布页让用户自己挑。

use serde::Serialize;

/// 移动端独立更新源，按顺序尝试，第一个能拿到合法 JSON 的就用（R2 主 / GitHub / Gitee 兜底）。
/// 跟桌面 `tauri.conf.json` → `plugins.updater.endpoints`（那是 `update.json`）是两套。
const UPDATE_JSON_ENDPOINTS: &[&str] = &[
    "https://pub-9d9e6c0cb6934fb0a0c505e3c64f39b2.r2.dev/knowledge-base/update-mobile.json",
    "https://gitee.com/bkywksj/knowledge-base-release/raw/master/update-mobile.json",
    "https://github.com/bkywksj/knowledge-base-release/raw/main/update-mobile.json",
];

/// 当 `update-mobile.json` 里没有可用的 APK URL 时，回落到 release 仓库的发布页，
/// 让用户自己挑 APK。
const RELEASE_PAGE_FALLBACK: &str = "https://gitee.com/bkywksj/knowledge-base-release/releases";

#[derive(Debug, Serialize)]
pub struct MobileUpdateInfo {
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: String,
    pub notes: String,
    /// APK 直链（优先）或 release 发布页（回落）
    pub download_url: String,
}

/// 简单版本号比较：把 "1.8.1" 拆成 [1,8,1] 逐段比，b > a 返回 true。
/// 非数字段当 0；段数不同短的补 0。够用了（本项目版本号一直是纯数字三段）。
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        s.trim_start_matches('v')
            .split('.')
            .map(|p| p.trim().parse::<u32>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(current), parse(latest));
    let n = a.len().max(b.len());
    for i in 0..n {
        let ai = a.get(i).copied().unwrap_or(0);
        let bi = b.get(i).copied().unwrap_or(0);
        if bi != ai {
            return bi > ai;
        }
    }
    false
}

/// 拉一个 endpoint 的 update.json，解析失败 / 网络失败都返回 None（让上层试下一个）。
async fn fetch_update_json(url: &str) -> Option<serde_json::Value> {
    let resp = reqwest::Client::new()
        .get(url)
        .header("User-Agent", "knowledge-base-mobile")
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<serde_json::Value>().await.ok()
}

#[tauri::command]
pub async fn check_mobile_update(app: tauri::AppHandle) -> Result<MobileUpdateInfo, String> {
    let current_version = app.package_info().version.to_string();

    // 依次尝试 3 个 endpoint
    let mut json: Option<serde_json::Value> = None;
    for ep in UPDATE_JSON_ENDPOINTS {
        if let Some(v) = fetch_update_json(ep).await {
            json = Some(v);
            break;
        }
    }
    let json = json.ok_or_else(|| "无法连接更新服务器（3 个源都失败），请检查网络".to_string())?;

    let latest_version = json
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "update-mobile.json 缺少 version 字段".to_string())?
        .to_string();
    let notes = json
        .get("notes")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // APK 直链：优先顶层 `url`（新版扁平结构）；兼容旧的 `platforms.android-arm64.url`
    // 嵌套写法；都没有则回落到 release 发布页让用户自己挑。
    let download_url = json
        .get("url")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            json.get("platforms")
                .and_then(|p| {
                    p.get("android-arm64")
                        .or_else(|| p.get("android-aarch64"))
                        .or_else(|| p.get("android"))
                })
                .and_then(|entry| entry.get("url"))
                .and_then(|u| u.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| RELEASE_PAGE_FALLBACK.to_string());

    Ok(MobileUpdateInfo {
        has_update: is_newer(&latest_version, &current_version),
        current_version,
        latest_version,
        notes,
        download_url,
    })
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn version_compare() {
        assert!(is_newer("1.8.2", "1.8.1"));
        assert!(is_newer("1.9.0", "1.8.9"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.8.1", "1.8.1"));
        assert!(!is_newer("1.8.0", "1.8.1"));
        assert!(is_newer("v1.8.2", "1.8.1")); // 容忍 v 前缀
        assert!(!is_newer("1.8", "1.8.0")); // 段数不同补 0
    }
}

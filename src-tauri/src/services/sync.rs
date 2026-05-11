//! 同步服务：导出/导入 ZIP 全量快照；WebDAV 推送/拉取
//!
//! V1/V2 设计：
//! - **全量快照**：每次导出/推送都生成完整 ZIP 包（app.db + 资产 + settings.json）
//! - **overwrite 模式**：导入时替换本地所有数据（先清空 → 再展开 ZIP）
//! - **merge 模式**：只添加 ZIP 里有、本地没有的资产；app.db 不合并（MVP 暂不实现真正合并，等同 overwrite）
//! - **密码**：WebDAV 密码 AES-256-GCM 加密后存入 SQLite app_config
//!   （密钥从 hostname + 固定 salt 派生；复制 db 到别的机器无法解密）

use std::fs;
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, Write};
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use crate::database::Database;
use crate::error::AppError;
use crate::models::{SyncImportMode, SyncManifest, SyncResult, SyncScope, SyncStats};
use crate::services::crypto;
use crate::services::webdav::WebDavClient;

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_FILE: &str = "manifest.json";
const DB_FILE_IN_ZIP: &str = "app.db";
const SETTINGS_FILE_IN_ZIP: &str = "settings.json";

pub struct SyncService;

impl SyncService {
    // ─── 导出 ──────────────────────────────────

    /// 把全量快照流式写入任意 `Write + Seek`（文件 / 内存缓冲）。
    ///
    /// 关键点：所有资产文件通过 `std::io::copy` 从磁盘直接拷进 ZipWriter，
    /// 不再一次性 `fs::read` 整份到内存。大知识库场景下内存峰值从 "资产总大小"
    /// 降到 "单个文件缓冲 + ZipWriter 压缩窗口" 级别，可避免 Mac 侧 GB 级占用。
    pub fn build_snapshot_to_writer<W: Write + Seek>(
        writer: W,
        data_dir: &Path,
        db: &Database,
        scope: &SyncScope,
        app_version: &str,
    ) -> Result<SyncStats, AppError> {
        let mut stats = SyncStats::default();
        let mut zip = ZipWriter::new(writer);
        let opt = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o644);

        // 1. app.db —— 用 VACUUM INTO 生成干净副本（绕开 WAL），然后流式复制进 ZIP
        if scope.notes {
            let tmp_db = data_dir.join(".sync-tmp-app.db");
            let _ = fs::remove_file(&tmp_db);
            db.vacuum_into(&tmp_db)?;
            zip.start_file(DB_FILE_IN_ZIP, opt)?;
            {
                let mut f = fs::File::open(&tmp_db)?;
                std::io::copy(&mut f, &mut zip)?;
            }
            let _ = fs::remove_file(&tmp_db);

            // 统计
            stats.notes_count = db.count_notes_active()?;
            stats.folders_count = db.count_folders()?;
            stats.tags_count = db.count_tags()?;
        }

        // 2. kb_assets/images/
        // 设计：ZIP 内路径**保留当前实例的 dev/prod 风格**（dev 写 dev-kb_assets/，
        // prod 写 kb_assets/），让 dev/prod 数据物理隔离不相互污染。
        // Import 端直接按 ZIP 内路径落盘，并通过 manifest.is_dev 做强一致性校验，
        // 跨 dev/prod 的导入会被拒绝（防止 dev 包污染 prod 实例反之亦然）。
        if scope.images {
            let images_dir = data_dir.join(assets_dir_name());
            let (count, size) = add_dir_to_zip(
                &mut zip,
                &images_dir,
                &format!("{}/", assets_dir_name()),
                opt,
            )?;
            stats.images_count = count;
            stats.assets_size += size;
        }

        // 3. pdfs/
        if scope.pdfs {
            let pdfs_dir = data_dir.join(pdfs_dir_name());
            let (count, size) =
                add_dir_to_zip(&mut zip, &pdfs_dir, &format!("{}/", pdfs_dir_name()), opt)?;
            stats.pdfs_count = count;
            stats.assets_size += size;
        }

        // 4. sources/
        if scope.sources {
            let sources_dir = data_dir.join(sources_dir_name());
            let (count, size) = add_dir_to_zip(
                &mut zip,
                &sources_dir,
                &format!("{}/", sources_dir_name()),
                opt,
            )?;
            stats.sources_count = count;
            stats.assets_size += size;
        }

        // 5. settings.json（通常很小，直接读）
        if scope.settings {
            let settings_file = data_dir.join(settings_file_name());
            if settings_file.exists() {
                zip.start_file(SETTINGS_FILE_IN_ZIP, opt)?;
                let mut f = fs::File::open(&settings_file)?;
                std::io::copy(&mut f, &mut zip)?;
            }
        }

        // 6. manifest.json
        let manifest = SyncManifest {
            schema_version: MANIFEST_VERSION,
            device: hostname::get()
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".into()),
            exported_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            app_version: app_version.to_string(),
            scope: scope.clone(),
            stats: stats.clone(),
            // 标记当前 build 类型，import 端做强一致性校验
            is_dev: Some(cfg!(debug_assertions)),
        };
        let manifest_json = serde_json::to_string_pretty(&manifest)?;
        zip.start_file(MANIFEST_FILE, opt)?;
        zip.write_all(manifest_json.as_bytes())?;

        zip.finish()?;
        Ok(stats)
    }

    /// 导出到本地文件（流式写盘，不占用对等内存）
    pub fn export_to_file(
        data_dir: &Path,
        db: &Database,
        scope: &SyncScope,
        app_version: &str,
        target_path: &Path,
    ) -> Result<SyncResult, AppError> {
        let file = fs::File::create(target_path)?;
        let writer = BufWriter::new(file);
        let stats = Self::build_snapshot_to_writer(writer, data_dir, db, scope, app_version)?;
        Ok(SyncResult {
            stats,
            finished_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        })
    }

    // ─── 导入 ──────────────────────────────────

    /// 从任意 `Read + Seek`（本地文件 / 内存游标）流式展开快照，避免把整个
    /// ZIP 载入内存。ZipArchive 需要 Seek，因此下载场景要先落盘到临时文件。
    pub fn apply_snapshot_from_reader<R: Read + Seek>(
        data_dir: &Path,
        db_path: &Path,
        reader: R,
        mode: SyncImportMode,
    ) -> Result<SyncManifest, AppError> {
        let mut archive = ZipArchive::new(reader)
            .map_err(|e| AppError::Custom(format!("解析 ZIP 失败: {}", e)))?;

        // 读取 manifest
        let manifest: SyncManifest = {
            let mut file = archive
                .by_name(MANIFEST_FILE)
                .map_err(|_| AppError::Custom("ZIP 缺少 manifest.json，不是合法的同步包".into()))?;
            let mut s = String::new();
            file.read_to_string(&mut s)?;
            serde_json::from_str(&s)?
        };

        if manifest.schema_version > MANIFEST_VERSION {
            return Err(AppError::Custom(format!(
                "同步包版本 {} 高于当前应用支持的 {}, 请升级应用",
                manifest.schema_version, MANIFEST_VERSION
            )));
        }

        // dev/prod 一致性校验：包里的 is_dev 必须和当前 build 匹配，
        // 防止 dev 包污染 prod 实例（资产路径前缀不同会造成无法读取的孤儿数据）。
        // is_dev 字段为 None = 老版本导出（在引入校验之前），按"宽容兼容"放行 + 日志告警。
        let current_is_dev = cfg!(debug_assertions);
        match manifest.is_dev {
            Some(zip_is_dev) if zip_is_dev != current_is_dev => {
                return Err(AppError::Custom(format!(
                    "同步包来源是 {} 实例，当前是 {} 实例，不允许跨环境导入（资产目录前缀不同会变成孤儿数据）",
                    if zip_is_dev { "dev" } else { "prod" },
                    if current_is_dev { "dev" } else { "prod" },
                )));
            }
            None => {
                log::warn!("[sync] 同步包未带 is_dev 字段（老版本导出），跳过 dev/prod 一致性校验");
            }
            _ => {}
        }

        // overwrite 模式：替换 app.db 前先清掉资产目录
        if matches!(mode, SyncImportMode::Overwrite) {
            if manifest.scope.images {
                let d = data_dir.join(assets_dir_name());
                if d.exists() {
                    let _ = fs::remove_dir_all(&d);
                }
                fs::create_dir_all(&d)?;
            }
            if manifest.scope.pdfs {
                let d = data_dir.join(pdfs_dir_name());
                if d.exists() {
                    let _ = fs::remove_dir_all(&d);
                }
                fs::create_dir_all(&d)?;
            }
            if manifest.scope.sources {
                let d = data_dir.join(sources_dir_name());
                if d.exists() {
                    let _ = fs::remove_dir_all(&d);
                }
                fs::create_dir_all(&d)?;
            }
        }

        // 展开 ZIP 所有文件
        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .map_err(|e| AppError::Custom(format!("读取 ZIP 条目失败: {}", e)))?;
            let name = file.name().to_string();

            if name == MANIFEST_FILE {
                continue;
            }

            let target = match name.as_str() {
                n if n == DB_FILE_IN_ZIP => {
                    // app.db 写入到传入的 db_path（可能是 dev- 前缀）
                    db_path.to_path_buf()
                }
                n if n == SETTINGS_FILE_IN_ZIP => data_dir.join(settings_file_name()),
                other => {
                    // 资产路径在 ZIP 内已带当前实例风格的 dev/prod 前缀
                    // （由 export 端 assets_dir_name() 等决定）+ 已通过 manifest.is_dev 校验
                    // 与当前 build 一致，直接 join 到 data_dir 落盘即可。
                    // 历史上有个 BUG：这里又加一遍 dev- 前缀导致 dev-dev-kb_assets/ 双前缀目录，
                    // 已通过 export/import 路径前缀职责对齐 + manifest 校验消除。
                    data_dir.join(other)
                }
            };

            if file.is_dir() {
                fs::create_dir_all(&target)?;
                continue;
            }

            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }

            // merge 模式：资产文件已存在则跳过（app.db / settings.json 总是覆盖）
            let is_asset = name != DB_FILE_IN_ZIP && name != SETTINGS_FILE_IN_ZIP;
            if is_asset && matches!(mode, SyncImportMode::Merge) && target.exists() {
                continue;
            }

            let mut out = fs::File::create(&target)?;
            std::io::copy(&mut file, &mut out)?;
        }

        // 同步完成后清理失效的 WebDAV 加密密码条目
        // （从别的设备同步来的密文，用的是那台设备的 hostname 派生的 key，本机解不开）
        Self::cleanup_invalid_webdav_passwords(db_path);

        Ok(manifest)
    }

    /// 扫描 app_config 中所有 sync.webdav_pw_enc.* 条目，
    /// 解密失败的（换设备后无效）直接删除。失败仅 warn，不阻塞同步。
    fn cleanup_invalid_webdav_passwords(db_path: &Path) {
        let conn = match rusqlite::Connection::open(db_path) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("清理失效密码：打开 DB 失败 {}", e);
                return;
            }
        };
        let entries: Vec<(String, String)> = match conn
            .prepare("SELECT key, value FROM app_config WHERE key LIKE 'sync.webdav_pw_enc.%'")
            .and_then(|mut stmt| {
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()
            }) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("清理失效密码：查询失败 {}", e);
                return;
            }
        };

        let mut removed = 0;
        for (key, enc) in entries {
            if crypto::decrypt(&enc).is_err() {
                if let Err(e) = conn.execute("DELETE FROM app_config WHERE key = ?1", [&key]) {
                    log::warn!("清理失效密码：删除 {} 失败 {}", key, e);
                } else {
                    removed += 1;
                }
            }
        }
        if removed > 0 {
            log::info!(
                "同步后已清理 {} 个失效的 WebDAV 密码条目（换设备导致）",
                removed
            );
        }
    }

    /// 从本地文件导入（流式读取，避免把整份 ZIP 载入内存）
    pub fn import_from_file(
        data_dir: &Path,
        db_path: &Path,
        source_path: &Path,
        mode: SyncImportMode,
    ) -> Result<SyncManifest, AppError> {
        let file = fs::File::open(source_path)?;
        let reader = BufReader::new(file);
        Self::apply_snapshot_from_reader(data_dir, db_path, reader, mode)
    }

    // ─── WebDAV 云同步 ──────────────────────────

    /// 推送到 WebDAV：先把快照流式写入临时文件，再流式上传，
    /// 全程不把整份 ZIP 驻留在内存中。
    pub async fn webdav_push(
        data_dir: &Path,
        db: &Database,
        scope: &SyncScope,
        app_version: &str,
        url: &str,
        username: &str,
        password: &str,
    ) -> Result<SyncResult, AppError> {
        let tmp_zip = data_dir.join(".sync-tmp-upload.zip");
        let _ = fs::remove_file(&tmp_zip);

        // 1. 流式构建快照到临时文件
        let stats = {
            let file = fs::File::create(&tmp_zip)?;
            let writer = BufWriter::new(file);
            Self::build_snapshot_to_writer(writer, data_dir, db, scope, app_version)?
        };

        // 2. 流式上传临时文件，无论成败都清理临时文件
        let filename = device_zip_name();
        let client = WebDavClient::new(url, username, password);
        let upload_result = client.upload_file(&filename, &tmp_zip).await;
        let _ = fs::remove_file(&tmp_zip);
        upload_result?;

        Ok(SyncResult {
            stats,
            finished_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        })
    }

    /// 从 WebDAV 拉取：先流式下载到临时文件，再流式解包，
    /// 避免把整份 ZIP 驻留在内存中。
    pub async fn webdav_pull(
        data_dir: &Path,
        db_path: &Path,
        mode: SyncImportMode,
        url: &str,
        username: &str,
        password: &str,
        preferred_filename: Option<&str>,
    ) -> Result<SyncManifest, AppError> {
        let client = WebDavClient::new(url, username, password);
        let filename = preferred_filename
            .map(|s| s.to_string())
            .unwrap_or_else(device_zip_name);

        let tmp_zip = data_dir.join(".sync-tmp-pull.zip");
        let _ = fs::remove_file(&tmp_zip);

        // 1. 流式下载到临时文件
        if let Err(e) = client.download_to_file(&filename, &tmp_zip).await {
            let _ = fs::remove_file(&tmp_zip);
            return Err(e);
        }

        // 2. 流式读取并展开
        let apply_result = (|| {
            let file = fs::File::open(&tmp_zip)?;
            let reader = BufReader::new(file);
            Self::apply_snapshot_from_reader(data_dir, db_path, reader, mode)
        })();

        let _ = fs::remove_file(&tmp_zip);
        apply_result
    }

    /// 列出云端所有 `kb-sync-*.zip` 快照（多设备场景）
    /// 返回 (filename, device_name) 元组列表，按设备名排序
    pub async fn webdav_list_snapshots(
        url: &str,
        username: &str,
        password: &str,
    ) -> Result<Vec<(String, String)>, AppError> {
        let client = WebDavClient::new(url, username, password);
        let files = client.list_files().await?;
        let mut snapshots: Vec<(String, String)> = files
            .into_iter()
            .filter(|f| f.starts_with("kb-sync-") && f.ends_with(".zip"))
            .map(|f| {
                // kb-sync-<device>.zip → 提取 <device>
                let device = f
                    .trim_start_matches("kb-sync-")
                    .trim_end_matches(".zip")
                    .to_string();
                (f, device)
            })
            .collect();
        snapshots.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(snapshots)
    }

    /// 预览云端 manifest（不下载资产，只读 manifest.json）
    pub async fn webdav_preview(
        url: &str,
        username: &str,
        password: &str,
        filename: Option<&str>,
    ) -> Result<SyncManifest, AppError> {
        let client = WebDavClient::new(url, username, password);
        let fname = filename
            .map(|s| s.to_string())
            .unwrap_or_else(device_zip_name);
        let bytes = client.download_bytes(&fname).await?;
        let reader = Cursor::new(bytes);
        let mut archive = ZipArchive::new(reader)
            .map_err(|e| AppError::Custom(format!("解析云端 ZIP 失败: {}", e)))?;
        let mut file = archive
            .by_name(MANIFEST_FILE)
            .map_err(|_| AppError::Custom("云端 ZIP 缺少 manifest.json".into()))?;
        let mut s = String::new();
        file.read_to_string(&mut s)?;
        let m: SyncManifest = serde_json::from_str(&s)?;
        Ok(m)
    }

    // ─── 密码存取（AES-GCM 加密 + SQLite app_config） ──────────────────────

    /// 配置 key：密文按用户名后缀区分（支持多 WebDAV 账号）
    /// 最终存的键形如 `sync.webdav_pw_enc.<username>`，value 是 base64 密文
    fn pw_config_key(username: &str) -> String {
        format!("sync.webdav_pw_enc.{}", username)
    }

    /// 把 WebDAV 密码加密后存入 SQLite
    pub fn save_webdav_password(
        db: &Database,
        username: &str,
        password: &str,
    ) -> Result<(), AppError> {
        let enc = crypto::encrypt(password)?;
        db.set_config(&Self::pw_config_key(username), &enc)?;
        Ok(())
    }

    /// 从 SQLite 读 WebDAV 密文并解密
    pub fn get_webdav_password(db: &Database, username: &str) -> Result<Option<String>, AppError> {
        match db.get_config(&Self::pw_config_key(username))? {
            Some(enc) if !enc.is_empty() => crypto::decrypt(&enc).map(Some),
            _ => Ok(None),
        }
    }

    /// 删除 SQLite 中的 WebDAV 密文
    pub fn delete_webdav_password(db: &Database, username: &str) -> Result<(), AppError> {
        let _ = db.delete_config(&Self::pw_config_key(username))?;
        Ok(())
    }

    // ─── 临时文件孤儿清理 ──────────────────────────
    //
    // 同步流程中会在 data_dir 下生成 `.sync-tmp-*` 临时文件（VACUUM 副本 / 上传 zip / 下载 zip）：
    //   - `.sync-tmp-app.db`     —— VACUUM INTO 临时副本（导出/推送）
    //   - `.sync-tmp-upload.zip` —— WebDAV 推送前的本地快照
    //   - `.sync-tmp-pull.zip`   —— WebDAV 下载落盘
    // 正常路径会在使用后清理，但应用崩溃 / kill 导致残留。本方法在启动期统一扫一遍。
    //
    // 安全保证：**严格匹配前缀 `.sync-tmp-` 且只看顶层文件**（不递归子目录），
    // 不会误删 `sources/` `pdfs/` `kb_assets/` 等业务资产，也不会动子目录里的同名文件。

    /// 扫 `data_dir` 顶层删除 `.sync-tmp-*` 残留文件，返回删除数量。
    /// 任意单个失败仅 warn，不阻塞启动。
    pub fn cleanup_orphan_temp_files(data_dir: &Path) -> usize {
        let entries = match fs::read_dir(data_dir) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("[sync] cleanup_orphan_temp_files: 读取 {} 失败 {}", data_dir.display(), e);
                return 0;
            }
        };
        let mut removed = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            // 只删顶层"文件"，绝不进子目录、绝不动目录本身
            let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
            if !is_file {
                continue;
            }
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if !name.starts_with(".sync-tmp-") {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(_) => {
                    log::info!("[sync] 启动清理临时孤儿文件: {}", path.display());
                    removed += 1;
                }
                Err(e) => {
                    log::warn!("[sync] 删除孤儿临时文件 {} 失败: {}", path.display(), e);
                }
            }
        }
        removed
    }

    // ─── 快照加密（T-S050 端到端加密备份；Part 1 核心层，集成到 push/pull/export 是 Part 2） ──

    /// 用密码加密快照 ZIP 字节。
    ///
    /// 输出格式：`[MAGIC 8B][salt 16B][nonce 12B + AES-256-GCM ciphertext + tag]`
    /// （后段就是 `crypto::aead_encrypt` 的输出 = `nonce ‖ ciphertext+tag`）
    /// - salt 公开存放在文件里是安全的（Argon2 设计如此）
    /// - **整块加密**：zip 字节全部进内存（大库 >100MB 注意内存峰值；快照归档通常阶段性手动操作）
    #[allow(dead_code)] // T-S050 Part 2 集成到 webdav_push / export_to_file 时启用
    pub fn encrypt_snapshot(zip: &[u8], password: &str) -> Result<Vec<u8>, AppError> {
        if password.is_empty() {
            return Err(AppError::Custom("备份密码不能为空".into()));
        }
        let salt = crypto::new_salt();
        let key = crypto::derive_user_key(password, &salt)?;
        let blob = crypto::aead_encrypt(&key, zip)?; // = nonce(12) ‖ ciphertext+tag
        let mut out = Vec::with_capacity(SNAPSHOT_MAGIC.len() + salt.len() + blob.len());
        out.extend_from_slice(SNAPSHOT_MAGIC);
        out.extend_from_slice(&salt);
        out.extend_from_slice(&blob);
        Ok(out)
    }

    /// 解密快照文件 → ZIP 字节。密码错误 / 文件损坏 / 魔数不匹配 → Err。
    #[allow(dead_code)] // T-S050 Part 2 集成到 webdav_pull / import_from_file 时启用
    pub fn decrypt_snapshot(enc: &[u8], password: &str) -> Result<Vec<u8>, AppError> {
        let min_len = SNAPSHOT_MAGIC.len() + crypto::SALT_LEN + crypto::NONCE_LEN + 16; // +16 GCM tag
        if enc.len() < min_len {
            return Err(AppError::Custom("加密快照文件太短或已损坏".into()));
        }
        if &enc[..SNAPSHOT_MAGIC.len()] != SNAPSHOT_MAGIC {
            return Err(AppError::Custom(
                "不是合法的加密快照（魔数不匹配；可能是明文 ZIP 或损坏文件）".into(),
            ));
        }
        let salt = &enc[SNAPSHOT_MAGIC.len()..SNAPSHOT_MAGIC.len() + crypto::SALT_LEN];
        let blob = &enc[SNAPSHOT_MAGIC.len() + crypto::SALT_LEN..];
        let key = crypto::derive_user_key(password, salt)?;
        crypto::aead_decrypt(&key, blob)
            .map_err(|_| AppError::Custom("备份密码错误，或文件已损坏".into()))
    }

    /// 检查字节流是否是加密快照（看魔数头）。明文 ZIP 以 `PK\x03\x04` 开头，不会误判。
    #[allow(dead_code)] // T-S050 Part 2 集成到 webdav_pull / import_from_file 时启用
    pub fn is_encrypted_snapshot(bytes: &[u8]) -> bool {
        bytes.len() >= SNAPSHOT_MAGIC.len() && &bytes[..SNAPSHOT_MAGIC.len()] == SNAPSHOT_MAGIC
    }

    // ─── 备份密码存取（与 WebDAV 密码同机制：hostname 派生 key + AES-GCM 存 app_config）─────

    #[allow(dead_code)] // T-S050 Part 2 启用
    fn backup_pw_config_key() -> &'static str {
        "sync.backup_pw_enc"
    }

    /// 把备份密码加密后存入 SQLite（换设备无法解密 → 需重新填）
    #[allow(dead_code)] // T-S050 Part 2 启用
    pub fn save_backup_password(db: &Database, password: &str) -> Result<(), AppError> {
        if password.is_empty() {
            return Err(AppError::Custom("备份密码不能为空".into()));
        }
        let enc = crypto::encrypt(password)?;
        db.set_config(Self::backup_pw_config_key(), &enc)?;
        Ok(())
    }

    /// 从 SQLite 读备份密码密文并解密；解不开（换设备）返回 None
    #[allow(dead_code)] // T-S050 Part 2 启用
    pub fn get_backup_password(db: &Database) -> Result<Option<String>, AppError> {
        match db.get_config(Self::backup_pw_config_key())? {
            Some(enc) if !enc.is_empty() => crypto::decrypt(&enc).map(Some),
            _ => Ok(None),
        }
    }

    /// 删除 SQLite 中的备份密码密文（关闭加密备份时调）
    #[allow(dead_code)] // T-S050 Part 2 启用
    pub fn delete_backup_password(db: &Database) -> Result<(), AppError> {
        let _ = db.delete_config(Self::backup_pw_config_key())?;
        Ok(())
    }
}

/// T-S050: 加密快照文件的魔数头（8 字节）
///
/// 用来识别"这是加密包"，与明文 ZIP（`PK\x03\x04` 开头）区分。
/// 后缀 `\0` 占位 + 版本位，未来格式变更时可用首字节区分。
#[allow(dead_code)] // T-S050 Part 2 启用
const SNAPSHOT_MAGIC: &[u8; 8] = b"KBSNCv1\0";

// ─── 辅助函数 ─────────────────────────────────

/// 把本地目录递归加入 ZIP，prefix 是 ZIP 内的路径前缀（需以 '/' 结尾）
/// 返回 (文件数, 总字节数)
///
/// 使用 `std::io::copy` 把文件内容流式喂给 ZipWriter，不再 `fs::read` 整份读入内存。
/// 这样即便单个资产是 GB 级大文件，内存占用也只是拷贝缓冲 + ZlibEncoder 窗口。
fn add_dir_to_zip<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    local_dir: &Path,
    prefix: &str,
    opt: SimpleFileOptions,
) -> Result<(usize, u64), AppError> {
    if !local_dir.exists() {
        return Ok((0, 0));
    }
    let mut count = 0;
    let mut size = 0u64;
    for entry in walkdir::WalkDir::new(local_dir).into_iter().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(local_dir)
            .map_err(|e| AppError::Custom(format!("路径拼接失败: {}", e)))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let zip_path = format!("{}{}", prefix, rel_str);
        zip.start_file(zip_path, opt)?;
        let mut f = fs::File::open(path)?;
        let n = std::io::copy(&mut f, zip)?;
        count += 1;
        size += n;
    }
    Ok((count, size))
}

fn assets_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "dev-kb_assets"
    } else {
        "kb_assets"
    }
}
fn pdfs_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "dev-pdfs"
    } else {
        "pdfs"
    }
}
fn sources_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "dev-sources"
    } else {
        "sources"
    }
}
fn settings_file_name() -> &'static str {
    if cfg!(debug_assertions) {
        "dev-settings.json"
    } else {
        "settings.json"
    }
}

/// 本机设备名作为云端 ZIP 文件名（同一 WebDAV 下多设备互不覆盖）
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    // ─── T-S050 快照加密 ─────────────────────────

    #[test]
    fn snapshot_encrypt_decrypt_roundtrip() {
        let zip_bytes = b"PK\x03\x04 fake zip content with some bytes \xff\x00\x42".to_vec();
        let pwd = "my-backup-pass-123";

        let enc = SyncService::encrypt_snapshot(&zip_bytes, pwd).unwrap();
        // 魔数头正确
        assert_eq!(&enc[..8], SNAPSHOT_MAGIC);
        // 密文比明文长（magic 8 + salt 16 + nonce 12 + tag 16 = 52 字节开销）
        assert_eq!(enc.len(), zip_bytes.len() + 8 + 16 + 12 + 16);
        // 是加密快照
        assert!(SyncService::is_encrypted_snapshot(&enc));
        // 明文 ZIP 不会被误判
        assert!(!SyncService::is_encrypted_snapshot(&zip_bytes));

        // 正确密码解密 → 还原原始字节
        let dec = SyncService::decrypt_snapshot(&enc, pwd).unwrap();
        assert_eq!(dec, zip_bytes);
    }

    #[test]
    fn snapshot_decrypt_wrong_password_fails() {
        let zip_bytes = b"some content".to_vec();
        let enc = SyncService::encrypt_snapshot(&zip_bytes, "right-pass").unwrap();
        let r = SyncService::decrypt_snapshot(&enc, "wrong-pass");
        assert!(r.is_err(), "错误密码必须解密失败");
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("密码") || msg.contains("损坏"), "错误信息应提示密码问题: {}", msg);
    }

    #[test]
    fn snapshot_decrypt_rejects_non_encrypted() {
        // 明文 ZIP（PK 头）传给 decrypt → 魔数不匹配（字节数要 ≥ 52 才不会先走"太短"分支）
        let plain_zip =
            b"PK\x03\x04 this is a plain zip file body with enough padding bytes here to exceed 52"
                .to_vec();
        assert!(plain_zip.len() >= 52);
        let r = SyncService::decrypt_snapshot(&plain_zip, "anypass");
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("魔数"), "got = {}", msg);
    }

    #[test]
    fn snapshot_decrypt_rejects_too_short() {
        let r = SyncService::decrypt_snapshot(b"KBSNCv1\0short", "p");
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("太短") || msg.contains("损坏"), "got = {}", msg);
    }

    #[test]
    fn snapshot_encrypt_empty_password_rejected() {
        assert!(SyncService::encrypt_snapshot(b"data", "").is_err());
    }

    #[test]
    fn snapshot_encrypt_nondeterministic() {
        // 同样输入加密两次 → salt/nonce 随机 → 密文不同（但都能解回）
        let data = b"deterministic input";
        let e1 = SyncService::encrypt_snapshot(data, "p").unwrap();
        let e2 = SyncService::encrypt_snapshot(data, "p").unwrap();
        assert_ne!(e1, e2, "随机 salt/nonce → 密文应不同");
        assert_eq!(SyncService::decrypt_snapshot(&e1, "p").unwrap(), data);
        assert_eq!(SyncService::decrypt_snapshot(&e2, "p").unwrap(), data);
    }

    #[test]
    fn backup_password_store_roundtrip() {
        let db = Database::init(":memory:").unwrap();
        // 初始无密码
        assert_eq!(SyncService::get_backup_password(&db).unwrap(), None);
        // 存
        SyncService::save_backup_password(&db, "backup-pw-xyz").unwrap();
        assert_eq!(
            SyncService::get_backup_password(&db).unwrap(),
            Some("backup-pw-xyz".into())
        );
        // 空密码拒绝
        assert!(SyncService::save_backup_password(&db, "").is_err());
        // 删
        SyncService::delete_backup_password(&db).unwrap();
        assert_eq!(SyncService::get_backup_password(&db).unwrap(), None);
    }

    /// 临时目录内放 4 类文件：3 个 `.sync-tmp-*` 应被删，2 个业务文件必须保留。
    /// 同时放一个同名前缀的子目录 + 子目录内同名前缀文件，验证"不递归子目录"。
    #[test]
    fn cleanup_orphan_temp_files_strict_prefix_only() {
        let tmp = std::env::temp_dir().join(format!(
            "kb-sync-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&tmp).unwrap();

        // 应删
        File::create(tmp.join(".sync-tmp-app.db")).unwrap();
        File::create(tmp.join(".sync-tmp-upload.zip")).unwrap();
        File::create(tmp.join(".sync-tmp-pull.zip")).unwrap();
        // 不应删（前缀近似但不完全匹配）
        File::create(tmp.join("sync-tmp-app.db")).unwrap(); // 缺前导点
        File::create(tmp.join("app.db")).unwrap(); // 业务数据库
        File::create(tmp.join("settings.json")).unwrap();
        File::create(tmp.join(".sync_tmp.bak")).unwrap(); // 下划线非连字符
        // 子目录及子目录内的同名前缀文件（不应被递归删除）
        let sub = tmp.join("pdfs");
        fs::create_dir_all(&sub).unwrap();
        File::create(sub.join(".sync-tmp-fake.zip")).unwrap();

        let removed = SyncService::cleanup_orphan_temp_files(&tmp);
        assert_eq!(removed, 3, "应只删顶层 3 个匹配文件");

        assert!(!tmp.join(".sync-tmp-app.db").exists());
        assert!(!tmp.join(".sync-tmp-upload.zip").exists());
        assert!(!tmp.join(".sync-tmp-pull.zip").exists());
        assert!(tmp.join("sync-tmp-app.db").exists(), "无点前缀不应删");
        assert!(tmp.join("app.db").exists(), "业务文件必须保留");
        assert!(tmp.join("settings.json").exists());
        assert!(tmp.join(".sync_tmp.bak").exists(), "下划线变体不应误删");
        assert!(sub.join(".sync-tmp-fake.zip").exists(), "子目录不应被递归扫描");

        // 收尾
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cleanup_returns_zero_for_nonexistent_dir() {
        let nowhere = std::path::PathBuf::from(format!(
            "{}/does-not-exist-{}",
            std::env::temp_dir().display(),
            std::process::id()
        ));
        let removed = SyncService::cleanup_orphan_temp_files(&nowhere);
        assert_eq!(removed, 0, "目录不存在时不应 panic，返回 0");
    }
}

fn device_zip_name() -> String {
    let device = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".into());
    // 清洗：只留字母/数字/-/_
    let safe: String = device
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("kb-sync-{}.zip", safe.to_lowercase())
}

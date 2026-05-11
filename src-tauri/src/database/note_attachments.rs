//! T-S020 sidecar CAS 附件同步索引表 DAO
//!
//! `note_attachments` (v37) 记录每条笔记引用的本地资产文件 + 内容 hash。
//! 用于：
//! - 同步 push 阶段：算出本机所有 unique sha256，与远端 has_attachment 算差集
//! - 同步 pull 阶段：远端 manifest.attachments 与本地 unique hashes 算差集，下载缺失的
//! - 本地引用查询：按 hash 反查本地 path（笔记内嵌引用 fallback 时用）

use rusqlite::{params, OptionalExtension};

use crate::error::AppError;

use super::Database;

/// 一条 note_attachments 行
#[derive(Debug, Clone)]
pub struct NoteAttachmentRow {
    pub note_id: i64,
    pub local_rel_path: String,
    pub sha256_hex: String,
    pub size: i64,
    pub mime: Option<String>,
}

impl Database {
    /// upsert 一条笔记 → 资产的引用记录（同 note_id+rel_path 覆盖）
    pub fn upsert_attachment_ref(
        &self,
        note_id: i64,
        local_rel_path: &str,
        sha256_hex: &str,
        size: i64,
        mime: Option<&str>,
    ) -> Result<(), AppError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AppError::Custom(e.to_string()))?;
        conn.execute(
            "INSERT INTO note_attachments (note_id, local_rel_path, sha256_hex, size, mime)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(note_id, local_rel_path) DO UPDATE SET
                 sha256_hex = excluded.sha256_hex,
                 size       = excluded.size,
                 mime       = excluded.mime",
            params![note_id, local_rel_path, sha256_hex, size, mime],
        )?;
        Ok(())
    }

    /// 列出某笔记的所有附件引用
    pub fn list_attachments_for_note(
        &self,
        note_id: i64,
    ) -> Result<Vec<NoteAttachmentRow>, AppError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AppError::Custom(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT note_id, local_rel_path, sha256_hex, size, mime
             FROM note_attachments WHERE note_id = ?1
             ORDER BY local_rel_path",
        )?;
        let rows = stmt
            .query_map([note_id], |row| {
                Ok(NoteAttachmentRow {
                    note_id: row.get(0)?,
                    local_rel_path: row.get(1)?,
                    sha256_hex: row.get(2)?,
                    size: row.get(3)?,
                    mime: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 列出全库所有 unique sha256 + 元数据（同步 push manifest 用）
    ///
    /// 重复 hash 取第一条（同一 hash 不同 path 是常见去重场景）。
    pub fn list_all_unique_attachments(&self) -> Result<Vec<NoteAttachmentRow>, AppError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AppError::Custom(e.to_string()))?;
        // GROUP BY sha256_hex 取每个 hash 的第一条（MIN(note_id) 保证稳定）
        let mut stmt = conn.prepare(
            "SELECT note_id, local_rel_path, sha256_hex, size, mime
             FROM note_attachments
             GROUP BY sha256_hex
             ORDER BY sha256_hex",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(NoteAttachmentRow {
                    note_id: row.get(0)?,
                    local_rel_path: row.get(1)?,
                    sha256_hex: row.get(2)?,
                    size: row.get(3)?,
                    mime: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 按 sha256 反查本地路径（任取一条）
    ///
    /// 同 hash 多 path 时返回第一条 — pull 端写入 `sync_in/<hash>.<ext>` 不依赖此查找；
    /// 此方法主要给"本地命中跳过下载"的场景用。
    pub fn find_attachment_path_by_hash(
        &self,
        sha256_hex: &str,
    ) -> Result<Option<String>, AppError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AppError::Custom(e.to_string()))?;
        let path = conn
            .query_row(
                "SELECT local_rel_path FROM note_attachments
                 WHERE sha256_hex = ?1 LIMIT 1",
                [sha256_hex],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(path)
    }

    /// 删除某笔记的全部附件引用（笔记物理删除时用；CASCADE 也会触发，但显式调更安全）
    #[allow(dead_code)]
    pub fn delete_attachment_refs_for_note(&self, note_id: i64) -> Result<usize, AppError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AppError::Custom(e.to_string()))?;
        let n = conn.execute(
            "DELETE FROM note_attachments WHERE note_id = ?1",
            [note_id],
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::NoteInput;

    fn setup() -> (Database, i64) {
        let db = Database::init(":memory:").expect("init :memory: 应成功");
        let n = db
            .create_note(&NoteInput {
                title: "笔记 A".into(),
                content: "x".into(),
                folder_id: None,
            })
            .unwrap();
        (db, n.id)
    }

    #[test]
    fn schema_creates_note_attachments_table() {
        let db = Database::init(":memory:").unwrap();
        let conn = db.conn_lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='note_attachments'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "v37 应创建 note_attachments 表");
    }

    #[test]
    fn upsert_and_list_for_note() {
        let (db, nid) = setup();
        db.upsert_attachment_ref(nid, "kb_assets/images/a.png", "h1", 100, Some("image/png"))
            .unwrap();
        db.upsert_attachment_ref(nid, "pdfs/b.pdf", "h2", 2000, Some("application/pdf"))
            .unwrap();

        let rows = db.list_attachments_for_note(nid).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].local_rel_path, "kb_assets/images/a.png");
        assert_eq!(rows[1].local_rel_path, "pdfs/b.pdf");
    }

    #[test]
    fn upsert_overwrites_same_path() {
        let (db, nid) = setup();
        db.upsert_attachment_ref(nid, "kb_assets/images/a.png", "h1", 100, None)
            .unwrap();
        // 同 path 但 hash 变了 → 应覆盖
        db.upsert_attachment_ref(nid, "kb_assets/images/a.png", "h2", 200, None)
            .unwrap();
        let rows = db.list_attachments_for_note(nid).unwrap();
        assert_eq!(rows.len(), 1, "同 note_id+path 应只有一行");
        assert_eq!(rows[0].sha256_hex, "h2");
        assert_eq!(rows[0].size, 200);
    }

    #[test]
    fn list_all_unique_dedups_by_hash() {
        let (db, n1) = setup();
        let n2 = db
            .create_note(&NoteInput {
                title: "笔记 B".into(),
                content: "y".into(),
                folder_id: None,
            })
            .unwrap()
            .id;

        // 同一 hash 在两个笔记里
        db.upsert_attachment_ref(n1, "kb_assets/images/a.png", "shared_h", 100, None)
            .unwrap();
        db.upsert_attachment_ref(n2, "kb_assets/images/dup.png", "shared_h", 100, None)
            .unwrap();
        // 不同 hash
        db.upsert_attachment_ref(n1, "pdfs/b.pdf", "other_h", 2000, None)
            .unwrap();

        let unique = db.list_all_unique_attachments().unwrap();
        assert_eq!(unique.len(), 2, "GROUP BY sha256_hex 应只剩 2 个唯一 hash");

        let hashes: Vec<&str> = unique.iter().map(|r| r.sha256_hex.as_str()).collect();
        assert!(hashes.contains(&"shared_h"));
        assert!(hashes.contains(&"other_h"));
    }

    #[test]
    fn find_by_hash_works() {
        let (db, nid) = setup();
        db.upsert_attachment_ref(nid, "pdfs/x.pdf", "abc", 1, None)
            .unwrap();
        assert_eq!(
            db.find_attachment_path_by_hash("abc").unwrap(),
            Some("pdfs/x.pdf".into())
        );
        assert_eq!(db.find_attachment_path_by_hash("none").unwrap(), None);
    }

    #[test]
    fn cascade_delete_when_note_deleted() {
        let (db, nid) = setup();
        db.upsert_attachment_ref(nid, "pdfs/x.pdf", "abc", 1, None)
            .unwrap();
        // 物理删除笔记应触发 CASCADE
        {
            let conn = db.conn_lock().unwrap();
            conn.execute("DELETE FROM notes WHERE id = ?1", [nid]).unwrap();
        }
        assert!(db.list_attachments_for_note(nid).unwrap().is_empty());
    }
}

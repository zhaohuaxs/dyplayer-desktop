use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Mutex;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sources(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    username TEXT NOT NULL DEFAULT '',
    root TEXT NOT NULL DEFAULT '',
    extra_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS play_history(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id INTEGER NOT NULL,
    path TEXT NOT NULL,
    title TEXT NOT NULL,
    position_sec REAL NOT NULL DEFAULT 0,
    duration_sec REAL NOT NULL DEFAULT 0,
    finished INTEGER NOT NULL DEFAULT 0,
    http_headers TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(source_id, path)
);
CREATE TABLE IF NOT EXISTS favorites(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id INTEGER NOT NULL,
    path TEXT NOT NULL,
    is_dir INTEGER NOT NULL DEFAULT 0,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(source_id, path)
);
CREATE TABLE IF NOT EXISTS media_library(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id INTEGER NOT NULL,
    folder_path TEXT NOT NULL,
    display_name TEXT NOT NULL,
    cover_path TEXT,
    cover_probed INTEGER NOT NULL DEFAULT 0,
    pinned INTEGER NOT NULL DEFAULT 0,
    added_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(source_id, folder_path)
);
CREATE TABLE IF NOT EXISTS danmaku_bindings(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id INTEGER NOT NULL,
    video_path TEXT NOT NULL,
    danmaku_path TEXT NOT NULL,
    offset_sec REAL NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(source_id, video_path, danmaku_path)
);
CREATE TABLE IF NOT EXISTS track_bindings(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id INTEGER NOT NULL,
    video_path TEXT NOT NULL,
    aid INTEGER,
    sid INTEGER,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(source_id, video_path)
);
CREATE TABLE IF NOT EXISTS media_index(
    source_id INTEGER NOT NULL,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(source_id, path)
);
CREATE TABLE IF NOT EXISTS app_meta(
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// 媒体索引搜索命中条目。
#[derive(Debug, Clone)]
pub struct IndexHit {
    pub path: String,
    pub name: String,
    pub size: u64,
}

/// LIKE 模式转义：% _ \ → 字面量（配 ESCAPE '\' 子句）。
fn escape_like(q: &str) -> String {
    q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// 线程安全封装：读写连接分离（WAL 模式下读不阻塞写、写不阻塞读）。
/// `write_conn` 用于 INSERT/UPDATE/DELETE/事务；`read_conn` 用于 SELECT。
/// 各自 Mutex 保证 Connection 的线程安全（rusqlite Connection 非 Sync）。
pub struct Store {
    write_conn: Mutex<Connection>,
    /// 读连接：文件 DB 用独立 Connection（WAL 并发读）；`:memory:` 无此能力
    ///（每 Connection 独立内存库），回退到 write_conn。
    read_conn: Option<Mutex<Connection>>,
}

impl Store {
    pub fn open_default() -> anyhow::Result<Self> {
        let dir = crate::config::AppConfig::data_dir()?;
        let db_path = dir.join("library.db");
        Self::open(&db_path)
    }

    pub fn open(path: &PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // WAL 模式：读不阻塞写、写不阻塞读（多读单写并发）。
        // synchronous=NORMAL：WAL 下事务持久性仍保证，减少 fsync 开销。
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        // 存量库迁移：media_library.cover_probed（CREATE TABLE IF NOT EXISTS
        // 对已有表不生效；列已存在时 "duplicate column name" 是正常情况，
        // 其他错误需告警，否则后续查询因缺列报错但迁移现场已丢失）。
        match conn.execute(
            "ALTER TABLE media_library ADD COLUMN cover_probed INTEGER NOT NULL DEFAULT 0",
            [],
        ) {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column") => {}
            Err(e) => tracing::warn!(target: "store", "迁移 media_library.cover_probed 失败: {e}"),
        }
        match conn.execute(
            "ALTER TABLE play_history ADD COLUMN http_headers TEXT",
            [],
        ) {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column") => {}
            Err(e) => tracing::warn!(target: "store", "迁移 play_history.http_headers 失败: {e}"),
        }
        // 读连接：独立 Connection 共享同一 DB 文件，WAL 下可并发读。
        // `:memory:` 每次打开是独立内存库，无法跨 Connection 共享数据，
        // 回退到 None（lock_read 走 write_conn）——仅测试场景。
        let is_memory = path.as_os_str() == ":memory:";
        let read_conn = if is_memory {
            None
        } else {
            let rc = Connection::open(path)?;
            rc.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
            Some(Mutex::new(rc))
        };
        Ok(Self {
            write_conn: Mutex::new(conn),
            read_conn,
        })
    }

    /// 持写锁获取连接。poison 恢复而非传染：持锁线程 panic 时取出内层数据
    /// 继续用——SQLite 事务完整性由 rusqlite 回滚保证，锁中毒把单次故障
    /// 放大为持续崩溃（DB 在 UI 帧内读取）只会更糟。
    fn lock_write(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.write_conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 持读锁获取连接（WAL 模式下与写连接并发不互斥）。
    /// `:memory:` 无独立读连接，回退到写连接（仅测试场景）。
    fn lock_read(&self) -> std::sync::MutexGuard<'_, Connection> {
        if let Some(rc) = &self.read_conn {
            rc.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        } else {
            self.lock_write()
        }
    }

    /// 在持写锁闭包内访问底层 SQLite 连接（跨线程共享的统一入口）。
    /// 调用方自行确保闭包内含写操作时走此入口；纯读优先用专用读方法。
    pub fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        f(&self.lock_write())
    }

    /// 插入媒体源并在同一锁段内返回新行 id（并发插入时防止错读他人 rowid）。
    pub fn add_source(
        &self,
        kind: &str,
        name: &str,
        endpoint: &str,
        username: &str,
        extra_json: &str,
    ) -> anyhow::Result<i64> {
        let conn = self.lock_write();
        conn.execute(
            "INSERT INTO sources(kind, name, endpoint, username, extra_json) \
             VALUES(?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![kind, name, endpoint, username, extra_json],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn upsert_play_position(
        &self,
        source_id: i64,
        path: &str,
        title: &str,
        position_sec: f64,
        duration_sec: f64,
        http_headers: Option<&str>,
    ) -> anyhow::Result<()> {
        let finished = if duration_sec > 0.0 {
            (position_sec / duration_sec) >= 0.98
        } else {
            false
        };
        self.lock_write().execute(
            "INSERT INTO play_history(source_id, path, title, position_sec, duration_sec, finished, http_headers, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))
             ON CONFLICT(source_id, path) DO UPDATE SET
                title=excluded.title,
                position_sec=excluded.position_sec,
                duration_sec=excluded.duration_sec,
                finished=excluded.finished,
                http_headers=excluded.http_headers,
                updated_at=excluded.updated_at",
            rusqlite::params![source_id, path, title, position_sec, duration_sec, finished as i64, http_headers],
        )?;
        Ok(())
    }

    pub fn mark_finished(&self, source_id: i64, path: &str) -> anyhow::Result<()> {
        self.lock_write().execute(
            "UPDATE play_history SET finished=1, position_sec=duration_sec, updated_at=datetime('now')
             WHERE source_id=?1 AND path=?2",
            rusqlite::params![source_id, path],
        )?;
        Ok(())
    }

    pub fn clear_play_history(&self) -> anyhow::Result<usize> {
        let deleted = self
            .lock_write()
            .execute("DELETE FROM play_history", [])?;
        Ok(deleted)
    }

    /// 删除单条播放记录（按 source_id + path 唯一定位）。
    pub fn delete_play_history(&self, source_id: i64, path: &str) -> anyhow::Result<usize> {
        let deleted = self.lock_write().execute(
            "DELETE FROM play_history WHERE source_id=?1 AND path=?2",
            rusqlite::params![source_id, path],
        )?;
        Ok(deleted)
    }

    /// 删除某媒体库文件夹下所有播放记录（path 前缀匹配 folder_path）。
    pub fn clear_play_history_for_folder(&self, folder_path: &str) -> anyhow::Result<usize> {
        let deleted = self.lock_write().execute(
            "DELETE FROM play_history WHERE path LIKE ?1 || '%'",
            rusqlite::params![folder_path],
        )?;
        Ok(deleted)
    }

    /// 删除所有属于媒体库的播放记录（path 匹配任一 folder_path 前缀）。
    pub fn clear_play_history_for_libs(
        &self,
        folder_paths: &[String],
    ) -> anyhow::Result<usize> {
        let conn = self.lock_write();
        let mut total = 0;
        for fp in folder_paths {
            total += conn.execute(
                "DELETE FROM play_history WHERE path LIKE ?1 || '%'",
                rusqlite::params![fp],
            )?;
        }
        Ok(total)
    }

    /// 删除所有不属于媒体库的播放记录（path 不匹配任何 folder_path 前缀）。
    pub fn clear_play_history_for_singles(
        &self,
        folder_paths: &[String],
    ) -> anyhow::Result<usize> {
        if folder_paths.is_empty() {
            return self.clear_play_history();
        }
        let conditions: Vec<String> = (0..folder_paths.len())
            .map(|i| format!("path NOT LIKE ?{} || '%'", i + 1))
            .collect();
        let sql = format!(
            "DELETE FROM play_history WHERE {}",
            conditions.join(" AND ")
        );
        let conn = self.lock_write();
        let mut stmt = conn.prepare(&sql)?;
        let deleted = stmt.execute(rusqlite::params_from_iter(
            folder_paths.iter().map(|s| s.as_str()),
        ))?;
        Ok(deleted)
    }

    /// 删除媒体源 + 级联清理（单事务）：play_history / favorites /
    /// media_library / media_index / danmaku_bindings。keyring 凭证由
    /// 调用方（app 层）自行清理（secrets 非 DB 域）。
    pub fn remove_source(&self, source_id: i64) -> anyhow::Result<()> {
        let conn = self.lock_write();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM sources WHERE id=?1",
            rusqlite::params![source_id],
        )?;
        tx.execute(
            "DELETE FROM play_history WHERE source_id=?1",
            rusqlite::params![source_id],
        )?;
        tx.execute(
            "DELETE FROM favorites WHERE source_id=?1",
            rusqlite::params![source_id],
        )?;
        tx.execute(
            "DELETE FROM media_library WHERE source_id=?1",
            rusqlite::params![source_id],
        )?;
        tx.execute(
            "DELETE FROM media_index WHERE source_id=?1",
            rusqlite::params![source_id],
        )?;
        tx.execute(
            "DELETE FROM danmaku_bindings WHERE source_id=?1",
            rusqlite::params![source_id],
        )?;
        tx.execute(
            "DELETE FROM track_bindings WHERE source_id=?1",
            rusqlite::params![source_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 更新媒体源定义（名称/地址/用户名/扩展配置）。root 与播放记录不受影响。
    pub fn update_source(
        &self,
        source_id: i64,
        kind: &str,
        name: &str,
        endpoint: &str,
        username: &str,
        extra_json: &str,
    ) -> anyhow::Result<()> {
        self.lock_write().execute(
            "UPDATE sources SET kind=?1, name=?2, endpoint=?3, username=?4, extra_json=?5,
             updated_at=datetime('now') WHERE id=?6",
            rusqlite::params![kind, name, endpoint, username, extra_json, source_id],
        )?;
        Ok(())
    }

    pub fn load_play_position(
        &self,
        source_id: i64,
        path: &str,
    ) -> anyhow::Result<Option<PlayRecord>> {
        let conn = self.lock_read();
        let mut stmt = conn.prepare(
            "SELECT title, position_sec, duration_sec, finished FROM play_history
             WHERE source_id=?1 AND path=?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![source_id, path])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(PlayRecord {
            title: row.get(0)?,
            position_sec: row.get(1)?,
            duration_sec: row.get(2)?,
            finished: row.get::<_, i64>(3)? != 0,
        }))
    }

    pub fn recent_played(&self, limit: i64) -> anyhow::Result<Vec<RecentEntry>> {
        let conn = self.lock_read();
        let mut stmt = conn.prepare(
            "SELECT source_id, path, title, position_sec, duration_sec, finished, http_headers, updated_at
             FROM play_history ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit], |row| {
            let parsed = (|| -> rusqlite::Result<RecentEntry> {
                Ok(RecentEntry {
                    source_id: row.get(0)?,
                    path: row.get(1)?,
                    title: row.get(2)?,
                    position_sec: row.get(3)?,
                    duration_sec: row.get(4)?,
                    finished: row.get::<_, i64>(5)? != 0,
                    http_headers: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })();
            if let Err(e) = &parsed {
                tracing::warn!("跳过无法解码的最近播放记录: {e}");
            }
            Ok(parsed.ok())
        })?;
        Ok(rows
            .filter_map(|r| match r {
                Ok(opt) => opt,
                Err(e) => {
                    tracing::warn!("读取最近播放记录失败: {e}");
                    None
                }
            })
            .collect())
    }

    /// 媒体库集数统计 join 源：某源下路径前缀内（含子层级）的全部播放
    /// 记录。分隔符归一 `/` 比较：`norm == prefix`（单文件条目）或
    /// `norm.starts_with(prefix + "/")`（文件夹条目，带边界防 `A` 误配
    /// `AB`）。内存过滤（单源记录量小，避免 LIKE 通配转义）。
    pub fn positions_under(
        &self,
        source_id: i64,
        prefix: &str,
    ) -> anyhow::Result<Vec<(String, f64, f64, bool)>> {
        let conn = self.lock_read();
        let mut stmt = conn.prepare(
            "SELECT path, position_sec, duration_sec, finished FROM play_history
              WHERE source_id=?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![source_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        })?;
        let folder = format!("{}/", prefix.replace('\\', "/"));
        let prefix = prefix.replace('\\', "/");
        Ok(rows
            .filter_map(|r| r.ok())
            .filter(|(path, _, _, _)| {
                let norm = path.replace('\\', "/");
                norm == prefix || norm.starts_with(&folder)
            })
            .collect())
    }

    /// 整源替换媒体索引（后台递归扫描完成后事务写入：DELETE + INSERT）。
    /// entries = (归一化 path, 文件名, size)。
    pub fn replace_media_index(
        &self,
        source_id: i64,
        entries: &[(String, String, u64)],
    ) -> anyhow::Result<usize> {
        let mut conn = self.lock_write();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM media_index WHERE source_id=?1", [source_id])?;
        let mut stmt =
            tx.prepare("INSERT OR REPLACE INTO media_index(source_id, path, name, size) VALUES(?1, ?2, ?3, ?4)")?;
        for (path, name, size) in entries {
            stmt.execute(rusqlite::params![source_id, path, name, *size as i64])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(entries.len())
    }

    /// 某源的索引条数（0 = 索引未建立：搜索回退"仅本列表筛选"）。
    pub fn media_index_count(&self, source_id: i64) -> anyhow::Result<i64> {
        let conn = self.lock_read();
        Ok(conn.query_row(
            "SELECT count(*) FROM media_index WHERE source_id=?1",
            [source_id],
            |row| row.get(0),
        )?)
    }

    /// 索引搜索：文件名/路径 LIKE 包含匹配（ASCII 大小写不敏感；% _ \
    /// 转义为字面量）。按名字自然序无关——直接按 path 排序稳定输出。
    pub fn search_media_index(
        &self,
        source_id: i64,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<IndexHit>> {
        let escaped = escape_like(query);
        let conn = self.lock_read();
        let mut stmt = conn.prepare(
            "SELECT path, name, size FROM media_index
              WHERE source_id=?1
                AND (name LIKE '%'||?2||'%' ESCAPE '\\' OR path LIKE '%'||?2||'%' ESCAPE '\\')
              ORDER BY path LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![source_id, escaped, limit as i64],
            |row| {
                Ok(IndexHit {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    size: row.get::<_, i64>(2)?.max(0) as u64,
                })
            },
        )?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 应用级 KV 元数据（如 last_index_date：媒体索引每日首启刷新标记）。
    pub fn get_meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.lock_read();
        let mut stmt = conn.prepare("SELECT value FROM app_meta WHERE key=?1")?;
        let mut rows = stmt.query([key])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.lock_write();
        conn.execute(
            "INSERT INTO app_meta(key, value) VALUES(?1, ?2)
              ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// 本地时区今天（YYYY-MM-DD，SQLite date('now','localtime')）：
    /// 媒体索引"每天首次启动刷新"判定用。
    pub fn local_date(&self) -> String {
        self.lock_read()
            .query_row("SELECT date('now','localtime')", [], |r| r.get(0))
            .unwrap_or_else(|e| {
                tracing::warn!("读取本地日期失败: {e}");
                String::new()
            })
    }

    /// 收藏/取消收藏（幂等）：已收藏则移除并返回 false，否则插入并返回 true。
    pub fn toggle_favorite(
        &self,
        source_id: i64,
        path: &str,
        title: &str,
        is_dir: bool,
    ) -> anyhow::Result<bool> {
        let conn = self.lock_write();
        let existing = conn.query_row(
            "SELECT id FROM favorites WHERE source_id=?1 AND path=?2",
            rusqlite::params![source_id, path],
            |_row| Ok(()),
        );
        match existing {
            Ok(()) => {
                conn.execute(
                    "DELETE FROM favorites WHERE source_id=?1 AND path=?2",
                    rusqlite::params![source_id, path],
                )?;
                Ok(false)
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                conn.execute(
                    "INSERT INTO favorites(source_id, path, is_dir, title) VALUES(?1, ?2, ?3, ?4)",
                    rusqlite::params![source_id, path, is_dir as i64, title],
                )?;
                Ok(true)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// 指定源内某路径是否已收藏（浏览列表行的星标态）。
    pub fn is_favorite(&self, source_id: i64, path: &str) -> bool {
        self.lock_read()
            .query_row(
                "SELECT 1 FROM favorites WHERE source_id=?1 AND path=?2",
                rusqlite::params![source_id, path],
                |_row| Ok(()),
            )
            .is_ok()
    }

    /// 加入媒体库（UI 仅对文件夹开放入口；历史遗留的单视频条目仍可
    /// 存于 folder_path，同源同路径重复添加 = 更新显示名与封面，语义幂等）。
    pub fn add_media_library(
        &self,
        source_id: i64,
        folder_path: &str,
        display_name: &str,
        cover_path: Option<&str>,
    ) -> anyhow::Result<()> {
        self.lock_write().execute(
            "INSERT INTO media_library(source_id, folder_path, display_name, cover_path, cover_probed, added_at)
             VALUES(?1, ?2, ?3, ?4, 1, datetime('now'))
             ON CONFLICT(source_id, folder_path) DO UPDATE SET
                display_name=excluded.display_name,
                cover_path=excluded.cover_path,
                cover_probed=excluded.cover_probed",
            rusqlite::params![source_id, folder_path, display_name, cover_path],
        )?;
        Ok(())
    }

    /// 从媒体库移除条目。
    pub fn remove_media_library(&self, id: i64) -> anyhow::Result<()> {
        self.lock_write().execute(
            "DELETE FROM media_library WHERE id=?1",
            rusqlite::params![id],
        )?;
        Ok(())
    }

    /// 媒体库条目列表（置顶优先，加入时间倒序）。
    pub fn list_media_library(&self) -> anyhow::Result<Vec<MediaLibEntry>> {
        let conn = self.lock_read();
        let mut stmt = conn.prepare(
            "SELECT id, source_id, folder_path, display_name, cover_path, cover_probed, pinned, added_at
             FROM media_library ORDER BY pinned DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(MediaLibEntry {
                id: row.get(0)?,
                source_id: row.get(1)?,
                folder_path: row.get(2)?,
                display_name: row.get(3)?,
                cover_path: row.get(4)?,
                cover_probed: row.get::<_, i64>(5)? != 0,
                pinned: row.get::<_, i64>(6)? != 0,
                added_at: row.get(7)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 置顶/取消置顶。
    pub fn set_media_library_pinned(&self, id: i64, pinned: bool) -> anyhow::Result<()> {
        self.lock_write().execute(
            "UPDATE media_library SET pinned=?2 WHERE id=?1",
            rusqlite::params![id, pinned as i64],
        )?;
        Ok(())
    }

    /// 收藏列表（按收藏时间倒序）。source_id 过滤为 None 时返回全部源的收藏。
    pub fn list_favorites(&self, source_id: Option<i64>) -> anyhow::Result<Vec<FavoriteEntry>> {
        let conn = self.lock_read();
        let mut stmt = conn.prepare(
            "SELECT source_id, path, is_dir, title, created_at
             FROM favorites WHERE (?1 IS NULL OR source_id=?1) ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![source_id], map_favorite_row)?;
        Ok(rows
            .filter_map(|r| match r {
                Ok(opt) => opt,
                Err(e) => {
                    tracing::warn!("读取收藏记录失败: {e}");
                    None
                }
            })
            .collect())
    }

    /// 记住/更新 (视频, 弹幕文件) 绑定及其弹幕延迟偏移（秒）。
    /// 偏移属于"该视频配该弹幕文件"的校准值，同一视频换弹幕源各自独立。
    pub fn upsert_danmaku_binding(
        &self,
        source_id: i64,
        video_path: &str,
        danmaku_path: &str,
        offset_sec: f64,
    ) -> anyhow::Result<()> {
        self.lock_write().execute(
            "INSERT INTO danmaku_bindings(source_id, video_path, danmaku_path, offset_sec, updated_at)
             VALUES(?1, ?2, ?3, ?4, datetime('now'))
             ON CONFLICT(source_id, video_path, danmaku_path) DO UPDATE SET
                offset_sec=excluded.offset_sec,
                updated_at=excluded.updated_at",
            rusqlite::params![source_id, video_path, danmaku_path, offset_sec],
        )?;
        Ok(())
    }

    /// 查询某 (视频, 弹幕文件) 对的延迟偏移；无绑定返回 None。
    pub fn load_danmaku_offset(
        &self,
        source_id: i64,
        video_path: &str,
        danmaku_path: &str,
    ) -> anyhow::Result<Option<f64>> {
        let conn = self.lock_read();
        let mut stmt = conn.prepare(
            "SELECT offset_sec FROM danmaku_bindings
             WHERE source_id=?1 AND video_path=?2 AND danmaku_path=?3",
        )?;
        let mut rows = stmt.query(rusqlite::params![source_id, video_path, danmaku_path])?;
        Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
    }

    /// 该视频最近一次绑定的弹幕文件路径（手动指定会被记住，下次优先于同名加载）。
    /// 返回 None 表示无绑定，按同名 xml 回退。本地源为绝对路径，远程源为 VFS 路径。
    pub fn load_bound_danmaku_path(
        &self,
        source_id: i64,
        video_path: &str,
    ) -> anyhow::Result<Option<String>> {
        let conn = self.lock_read();
        let mut stmt = conn.prepare(
            "SELECT danmaku_path FROM danmaku_bindings
             WHERE source_id=?1 AND video_path=?2
             ORDER BY updated_at DESC, id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(rusqlite::params![source_id, video_path])?;
        Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
    }

    /// 记录用户在轨道菜单的显式选择（(源, 视频) 对级持久化，同
    /// danmaku_bindings 哲学）：下次播放同一视频时优先于语言偏好恢复。
    /// `value`：None = 显式关闭（存 0）；Some(id) = mpv track id。
    /// 单列 upsert——写 aid 不触碰既有 sid（VALUES 另一侧给 NULL，
    /// ON CONFLICT 只 SET 目标列）。
    pub fn save_track_selection(
        &self,
        source_id: i64,
        video_path: &str,
        kind: TrackKind,
        value: Option<i64>,
    ) -> anyhow::Result<()> {
        let v = value.unwrap_or(0);
        match kind {
            TrackKind::Audio => self.lock_write().execute(
                "INSERT INTO track_bindings(source_id, video_path, aid, updated_at)
                 VALUES(?1, ?2, ?3, datetime('now'))
                 ON CONFLICT(source_id, video_path) DO UPDATE SET
                    aid=excluded.aid, updated_at=excluded.updated_at",
                rusqlite::params![source_id, video_path, v],
            )?,
            TrackKind::Subtitle => self.lock_write().execute(
                "INSERT INTO track_bindings(source_id, video_path, sid, updated_at)
                 VALUES(?1, ?2, ?3, datetime('now'))
                 ON CONFLICT(source_id, video_path) DO UPDATE SET
                    sid=excluded.sid, updated_at=excluded.updated_at",
                rusqlite::params![source_id, video_path, v],
            )?,
        };
        Ok(())
    }

    /// 该视频的轨道选择绑定 (aid, sid)；无行返回 None，列未写为 None。
    /// 值语义：None = 无记录；Some(0) = 显式关闭；Some(id>0) = track id。
    pub fn load_track_binding(
        &self,
        source_id: i64,
        video_path: &str,
    ) -> anyhow::Result<Option<(Option<i64>, Option<i64>)>> {
        let conn = self.lock_read();
        let mut stmt = conn.prepare(
            "SELECT aid, sid FROM track_bindings
             WHERE source_id=?1 AND video_path=?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![source_id, video_path])?;
        Ok(rows
            .next()?
            .map(|row| -> rusqlite::Result<_> { Ok((row.get(0)?, row.get(1)?)) })
            .transpose()?)
    }
}

/// 轨道选择维度（音轨/字幕轨），与 [track_bindings] 表的 aid/sid 列
/// 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Subtitle,
}

fn map_favorite_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<FavoriteEntry>> {
    let parsed = (|| -> rusqlite::Result<FavoriteEntry> {
        Ok(FavoriteEntry {
            source_id: row.get(0)?,
            path: row.get(1)?,
            is_dir: row.get::<_, i64>(2)? != 0,
            title: row.get(3)?,
            created_at: row.get(4)?,
        })
    })();
    if let Err(e) = &parsed {
        tracing::warn!("跳过无法解码的收藏记录: {e}");
    }
    Ok(parsed.ok())
}

#[derive(Debug, Clone)]
pub struct PlayRecord {
    pub title: String,
    pub position_sec: f64,
    pub duration_sec: f64,
    pub finished: bool,
}

#[derive(Debug, Clone)]
pub struct FavoriteEntry {
    pub source_id: i64,
    pub path: String,
    pub is_dir: bool,
    pub title: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct RecentEntry {
    pub source_id: i64,
    pub path: String,
    pub title: String,
    pub position_sec: f64,
    pub duration_sec: f64,
    pub finished: bool,
    pub http_headers: Option<String>,
    pub updated_at: String,
}

/// 媒体库条目：源内文件夹 + 补充信息（显示名/封面/置顶）。
/// 进度与已看集数不落表，由 play_history 按 folder_path 前缀聚合推导。
#[derive(Debug, Clone)]
pub struct MediaLibEntry {
    pub id: i64,
    pub source_id: i64,
    pub folder_path: String,
    pub display_name: String,
    /// 约定封面（cover.jpg/poster.jpg/folder.jpg/cover.png）的完整 VFS 路径；None=占位卡。
    pub cover_path: Option<String>,
    /// 封面探测是否已完成（0=未探测，需后台补探测；1=已探测，cover_path 即终态）。
    pub cover_probed: bool,
    pub pinned: bool,
    pub added_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时文件 DB（WAL 读写连接分离需文件共享；:memory: 各连接独立不可见）。
    fn memory_store() -> Store {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "dyplayer_test_{}_{}.db",
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
        Store::open(&path).unwrap()
    }

    #[test]
    fn upsert_then_load_roundtrip() {
        let store = memory_store();
        store
            .upsert_play_position(1, "a.mp4", "A", 30.0, 100.0, None)
            .unwrap();
        let rec = store.load_play_position(1, "a.mp4").unwrap().unwrap();
        assert_eq!(rec.title, "A");
        assert_eq!(rec.position_sec, 30.0);
        assert_eq!(rec.duration_sec, 100.0);
        assert!(!rec.finished);
    }

    #[test]
    fn upsert_updates_existing_row() {
        let store = memory_store();
        store
            .upsert_play_position(1, "a.mp4", "A", 10.0, 100.0, None)
            .unwrap();
        store
            .upsert_play_position(1, "a.mp4", "A2", 50.0, 100.0, None)
            .unwrap();
        let rec = store.load_play_position(1, "a.mp4").unwrap().unwrap();
        assert_eq!(rec.title, "A2");
        assert_eq!(rec.position_sec, 50.0);
    }

    #[test]
    fn load_missing_returns_none() {
        let store = memory_store();
        assert!(
            store
                .load_play_position(1, "missing.mp4")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn media_library_add_list_remove_roundtrip() {
        let store = memory_store();
        store
            .add_media_library(
                1,
                "D:/Media/Relife",
                "Relife",
                Some("D:/Media/Relife/cover.jpg"),
            )
            .unwrap();
        store
            .add_media_library(2, "/anime/Conan", "名侦探柯南", None)
            .unwrap();
        let list = store.list_media_library().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].display_name, "名侦探柯南"); // id 倒序：后加在前
        assert!(
            list[1]
                .cover_path
                .as_deref()
                .is_some_and(|p| p.ends_with("cover.jpg"))
        );

        // 重复添加同源同路径 = 更新而非新增。
        store
            .add_media_library(1, "D:/Media/Relife", "ReLIFE 完结篇", None)
            .unwrap();
        let list = store.list_media_library().unwrap();
        assert_eq!(list.len(), 2);
        let relife = list.iter().find(|e| e.source_id == 1).unwrap();
        assert_eq!(relife.display_name, "ReLIFE 完结篇");
        assert!(relife.cover_path.is_none(), "重复添加未带封面时应清空封面");

        store.remove_media_library(list[0].id).unwrap();
        let list = store.list_media_library().unwrap();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn media_library_pinned_sorts_first() {
        let store = memory_store();
        store.add_media_library(1, "a", "A", None).unwrap();
        store.add_media_library(1, "b", "B", None).unwrap();
        store.add_media_library(1, "c", "C", None).unwrap();
        let list = store.list_media_library().unwrap();
        let id_b = list.iter().find(|e| e.folder_path == "b").unwrap().id;
        store.set_media_library_pinned(id_b, true).unwrap();
        let list = store.list_media_library().unwrap();
        assert_eq!(list[0].folder_path, "b");
        assert!(list[0].pinned);
    }

    #[test]
    fn remove_source_cascade_deletes_media_library() {
        let store = memory_store();
        let sid = store.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint) VALUES('local', 's', 'D:\\s')",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        });
        store.add_media_library(sid, "D:/s/x", "X", None).unwrap();
        store.remove_source(sid).unwrap();
        assert!(store.list_media_library().unwrap().is_empty());
    }

    #[test]
    fn remove_source_deletes_source_and_related_records() {
        let store = memory_store();
        let sid = store.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint) VALUES('local', 's', 'D:\\s')",
                [],
            )
            .unwrap();
            let sid: i64 = conn
                .query_row("SELECT id FROM sources", [], |r| r.get(0))
                .unwrap();
            conn.execute(
                "INSERT INTO play_history(source_id, path, title) VALUES(?1, 'a.mp4', 'A')",
                rusqlite::params![sid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO favorites(source_id, path, title) VALUES(?1, 'a.mp4', 'A')",
                rusqlite::params![sid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO media_index(source_id, path, name) VALUES(?1, 'D:/s/a.mp4', 'a.mp4')",
                rusqlite::params![sid],
            )
            .unwrap();
            sid
        });
        store.remove_source(sid).unwrap();
        let count = |sql: &str| {
            store
                .with_connection(|conn| {
                    conn.query_row(sql, rusqlite::params![sid], |r| r.get::<_, i64>(0))
                })
                .unwrap()
        };
        assert_eq!(count("SELECT count(*) FROM sources WHERE id=?1"), 0);
        assert_eq!(
            count("SELECT count(*) FROM play_history WHERE source_id=?1"),
            0
        );
        assert_eq!(
            count("SELECT count(*) FROM favorites WHERE source_id=?1"),
            0
        );
        assert_eq!(
            count("SELECT count(*) FROM media_index WHERE source_id=?1"),
            0
        );
    }

    #[test]
    fn update_source_rewrites_fields_and_keeps_root() {
        let store = memory_store();
        let sid = store.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint, username, root, extra_json) \
                 VALUES('webdav', 'old', 'http://old/dav', 'u1', '/video', '{}')",
                [],
            )
            .unwrap();
            let sid: i64 = conn
                .query_row("SELECT id FROM sources", [], |r| r.get(0))
                .unwrap();
            conn.execute(
                "INSERT INTO play_history(source_id, path, title) VALUES(?1, 'a.mp4', 'A')",
                rusqlite::params![sid],
            )
            .unwrap();
            sid
        });

        store
            .update_source(
                sid,
                "webdav",
                "nas",
                "http://new/dav",
                "guest",
                r#"{"accept_invalid_certs":true}"#,
            )
            .unwrap();

        let (kind, name, endpoint, username, root, extra): (
            String,
            String,
            String,
            String,
            String,
            String,
        ) = store
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT kind, name, endpoint, username, root, extra_json FROM sources WHERE id=?1",
                    rusqlite::params![sid],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
            })
            .unwrap();
        assert_eq!(kind, "webdav");
        assert_eq!(name, "nas");
        assert_eq!(endpoint, "http://new/dav");
        assert_eq!(username, "guest");
        // root 与播放记录不受编辑影响
        assert_eq!(root, "/video");
        assert!(extra.contains("accept_invalid_certs"));
        let history: i64 = store
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM play_history WHERE source_id=?1",
                    rusqlite::params![sid],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(history, 1);
    }

    #[test]
    fn update_source_missing_id_is_noop() {
        let store = memory_store();
        store
            .update_source(999, "webdav", "x", "http://x", "u", "{}")
            .unwrap();
    }

    #[test]
    fn near_end_marks_finished() {
        let store = memory_store();
        store
            .upsert_play_position(1, "a.mp4", "A", 98.0, 100.0, None)
            .unwrap();
        let rec = store.load_play_position(1, "a.mp4").unwrap().unwrap();
        assert!(rec.finished);
    }

    #[test]
    fn mark_finished_sets_flag_and_position() {
        let store = memory_store();
        store
            .upsert_play_position(2, "b.mkv", "B", 5.0, 60.0, None)
            .unwrap();
        store.mark_finished(2, "b.mkv").unwrap();
        let rec = store.load_play_position(2, "b.mkv").unwrap().unwrap();
        assert!(rec.finished);
        assert_eq!(rec.position_sec, 60.0);
    }

    #[test]
    fn recent_played_respects_limit() {
        let store = memory_store();
        store
            .upsert_play_position(1, "a.mp4", "A", 1.0, 10.0, None)
            .unwrap();
        store
            .upsert_play_position(1, "b.mp4", "B", 2.0, 10.0, None)
            .unwrap();
        store
            .upsert_play_position(1, "c.mp4", "C", 3.0, 10.0, None)
            .unwrap();
        let recent = store.recent_played(2).unwrap();
        assert_eq!(recent.len(), 2);
        assert!(recent.iter().all(|e| e.source_id == 1));
    }

    #[test]
    fn recent_played_skips_invalid_utf8_rows() {
        let store = memory_store();
        store
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO play_history(source_id, path, title, position_sec, duration_sec, finished, updated_at)
                     VALUES(1, 'bad.mp4', cast(x'4142FF' AS TEXT), 1.0, 10.0, 0, datetime('now'))",
                    [],
                )
            })
            .unwrap();
        store
            .upsert_play_position(1, "good.mp4", "Good", 2.0, 10.0, None)
            .unwrap();
        let recent = store.recent_played(10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].title, "Good");
        assert_eq!(recent[0].path, "good.mp4");
    }

    #[test]
    fn recent_played_all_invalid_rows_returns_empty() {
        let store = memory_store();
        store
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO play_history(source_id, path, title, position_sec, duration_sec, finished, updated_at)
                     VALUES(1, 'bad.mp4', cast(x'FF' AS TEXT), 1.0, 10.0, 0, datetime('now'))",
                    [],
                )
            })
            .unwrap();
        let recent = store.recent_played(10).unwrap();
        assert!(recent.is_empty());
    }

    #[test]
    fn clear_play_history_deletes_all_and_reports_count() {
        let store = memory_store();
        store
            .upsert_play_position(1, "a.mp4", "A", 1.0, 10.0, None)
            .unwrap();
        store
            .upsert_play_position(2, "b.mp4", "B", 2.0, 10.0, None)
            .unwrap();
        let deleted = store.clear_play_history().unwrap();
        assert_eq!(deleted, 2);
        assert!(store.recent_played(10).unwrap().is_empty());
        assert!(store.load_play_position(1, "a.mp4").unwrap().is_none());
        let deleted_again = store.clear_play_history().unwrap();
        assert_eq!(deleted_again, 0);
    }

    #[test]
    fn positions_under_prefix_boundary_and_normalization() {
        let store = memory_store();
        store
            .upsert_play_position(7, "D:/Media/A/EP01.mkv", "A1", 50.0, 100.0, None)
            .unwrap();
        store
            .upsert_play_position(7, "D:\\Media\\A\\EP02.mkv", "A2", 100.0, 100.0, None)
            .unwrap();
        // 同前缀不同目录（边界：A 不应匹配 AB/）。
        store
            .upsert_play_position(7, "D:/Media/AB/EP01.mkv", "AB1", 50.0, 100.0, None)
            .unwrap();
        // 其他源。
        store
            .upsert_play_position(8, "D:/Media/A/EP03.mkv", "A3", 50.0, 100.0, None)
            .unwrap();
        let rs = store.positions_under(7, "D:/Media/A").unwrap();
        assert_eq!(rs.len(), 2);
        assert!(rs.iter().any(|(p, _, _, _)| p == "D:/Media/A/EP01.mkv"));
        assert!(rs.iter().any(|(p, _, _, f)| p == "D:\\Media\\A\\EP02.mkv" && *f));
        // 前缀反斜杠形式归一后等价。
        assert_eq!(store.positions_under(7, "D:\\Media\\A").unwrap().len(), 2);
        // 单文件条目：精确匹配（不匹配同前缀其他文件）。
        assert_eq!(
            store.positions_under(7, "D:/Media/A/EP01.mkv").unwrap().len(),
            1
        );
        // 无记录前缀。
        assert!(store.positions_under(9, "D:/Media/A").unwrap().is_empty());
    }

    #[test]
    fn media_index_replace_search_and_count() {
        let store = memory_store();
        assert_eq!(store.media_index_count(5).unwrap(), 0);
        let entries = vec![
            ("D:/shows/A/EP01.mkv".to_string(), "EP01.mkv".to_string(), 100u64),
            ("D:/shows/A/sub/EP02.mkv".to_string(), "EP02.mkv".to_string(), 200u64),
        ];
        assert_eq!(store.replace_media_index(5, &entries).unwrap(), 2);
        assert_eq!(store.media_index_count(5).unwrap(), 2);
        // 其他源隔离。
        assert_eq!(store.media_index_count(6).unwrap(), 0);
        // 名字匹配（ASCII 大小写不敏感）。
        let hits = store.search_media_index(5, "ep01", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "EP01.mkv");
        assert_eq!(hits[0].size, 100);
        // 路径匹配（含子目录段）。
        assert_eq!(store.search_media_index(5, "sub", 10).unwrap().len(), 1);
        // 整源替换（旧行清除）。
        let entries2 = vec![("D:/movies/m.mkv".to_string(), "m.mkv".to_string(), 5u64)];
        assert_eq!(store.replace_media_index(5, &entries2).unwrap(), 1);
        assert_eq!(store.media_index_count(5).unwrap(), 1);
        assert!(store.search_media_index(5, "EP0", 10).unwrap().is_empty());
        // limit 生效。
        assert_eq!(store.search_media_index(5, "m", 0).unwrap().len(), 0);
    }

    #[test]
    fn media_index_search_escapes_like_wildcards() {
        let store = memory_store();
        let entries = vec![
            ("D:/a/100%cool.mkv".to_string(), "100%cool.mkv".to_string(), 1u64),
            ("D:/a/under_score.mkv".to_string(), "under_score.mkv".to_string(), 1u64),
            ("D:/a/plain.mkv".to_string(), "plain.mkv".to_string(), 1u64),
        ];
        store.replace_media_index(3, &entries).unwrap();
        // '%' 作为字面量：只命中名字含 % 的条目，而非全部。
        assert_eq!(store.search_media_index(3, "%", 10).unwrap().len(), 1);
        // '_' 作为字面量：只命中 under_score，而非全部。
        assert_eq!(store.search_media_index(3, "_", 10).unwrap().len(), 1);
        assert_eq!(store.search_media_index(3, "under_", 10).unwrap().len(), 1);
    }

    #[test]
    fn app_meta_roundtrip_and_local_date() {
        let store = memory_store();
        assert!(store.get_meta("last_index_date").unwrap().is_none());
        store.set_meta("last_index_date", "2026-10-21").unwrap();
        assert_eq!(
            store.get_meta("last_index_date").unwrap().as_deref(),
            Some("2026-10-21")
        );
        // upsert 语义。
        store.set_meta("last_index_date", "2026-10-22").unwrap();
        assert_eq!(
            store.get_meta("last_index_date").unwrap().as_deref(),
            Some("2026-10-22")
        );
        // 本地日期格式 YYYY-MM-DD（长度 10）。
        assert_eq!(store.local_date().len(), 10);
    }


    #[test]
    fn toggle_favorite_adds_then_removes() {
        let store = memory_store();
        assert!(!store.is_favorite(1, "/movies/ep01.mkv"));
        let added = store
            .toggle_favorite(1, "/movies/ep01.mkv", "ep01.mkv", false)
            .unwrap();
        assert!(added);
        assert!(store.is_favorite(1, "/movies/ep01.mkv"));
        // 重复 toggle = 取消
        let removed = store
            .toggle_favorite(1, "/movies/ep01.mkv", "ep01.mkv", false)
            .unwrap();
        assert!(!removed);
        assert!(!store.is_favorite(1, "/movies/ep01.mkv"));
    }

    #[test]
    fn favorites_scoped_by_source_and_dir_flag() {
        let store = memory_store();
        store.toggle_favorite(1, "/movies", "电影", true).unwrap();
        store
            .toggle_favorite(1, "/movies/a.mkv", "a.mkv", false)
            .unwrap();
        store
            .toggle_favorite(2, "/movies/a.mkv", "a.mkv", false)
            .unwrap();
        let all = store.list_favorites(None).unwrap();
        assert_eq!(all.len(), 3);
        let of_one = store.list_favorites(Some(1)).unwrap();
        assert_eq!(of_one.len(), 2);
        assert!(of_one.iter().any(|f| f.is_dir && f.title == "电影"));
        assert!(
            of_one
                .iter()
                .any(|f| !f.is_dir && f.path == "/movies/a.mkv")
        );
        let of_two = store.list_favorites(Some(2)).unwrap();
        assert_eq!(of_two.len(), 1);
        assert_eq!(of_two[0].source_id, 2);
    }

    #[test]
    fn remove_source_cascades_favorites() {
        let store = memory_store();
        let sid = store.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint) VALUES('local', 's', 'D:\\s')",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        });
        store
            .toggle_favorite(sid, "/x/a.mkv", "a.mkv", false)
            .unwrap();
        assert!(store.is_favorite(sid, "/x/a.mkv"));
        store.remove_source(sid).unwrap();
        assert!(!store.is_favorite(sid, "/x/a.mkv"));
        assert!(store.list_favorites(Some(sid)).unwrap().is_empty());
    }

    #[test]
    fn danmaku_binding_upsert_then_load_offset() {
        let store = memory_store();
        assert_eq!(
            store.load_danmaku_offset(1, "a.mkv", "a.xml").unwrap(),
            None
        );
        store
            .upsert_danmaku_binding(1, "a.mkv", "a.xml", -12.5)
            .unwrap();
        assert_eq!(
            store.load_danmaku_offset(1, "a.mkv", "a.xml").unwrap(),
            Some(-12.5)
        );
        // upsert 更新而非新增
        store
            .upsert_danmaku_binding(1, "a.mkv", "a.xml", 3.0)
            .unwrap();
        assert_eq!(
            store.load_danmaku_offset(1, "a.mkv", "a.xml").unwrap(),
            Some(3.0)
        );
    }

    #[test]
    fn danmaku_binding_offsets_are_per_pair() {
        let store = memory_store();
        // 同一视频换弹幕源：偏移各自独立
        store
            .upsert_danmaku_binding(1, "a.mkv", "a.xml", -5.0)
            .unwrap();
        store
            .upsert_danmaku_binding(1, "a.mkv", "other.xml", 120.0)
            .unwrap();
        assert_eq!(
            store.load_danmaku_offset(1, "a.mkv", "a.xml").unwrap(),
            Some(-5.0)
        );
        assert_eq!(
            store.load_danmaku_offset(1, "a.mkv", "other.xml").unwrap(),
            Some(120.0)
        );
        // 不同视频同名弹幕互不影响
        store
            .upsert_danmaku_binding(1, "b.mkv", "a.xml", 9.0)
            .unwrap();
        assert_eq!(
            store.load_danmaku_offset(1, "b.mkv", "a.xml").unwrap(),
            Some(9.0)
        );
        // 不同源同路径互不影响
        assert_eq!(
            store.load_danmaku_offset(2, "a.mkv", "a.xml").unwrap(),
            None
        );
    }

    #[test]
    fn load_bound_danmaku_path_returns_latest_binding() {
        let store = memory_store();
        assert_eq!(
            store.load_bound_danmaku_path(1, "a.mkv").unwrap(),
            None
        );
        store
            .upsert_danmaku_binding(1, "a.mkv", "a.xml", 0.0)
            .unwrap();
        assert_eq!(
            store.load_bound_danmaku_path(1, "a.mkv").unwrap(),
            Some("a.xml".into())
        );
        // 后绑定的优先（updated_at/id 倒序）
        store
            .upsert_danmaku_binding(1, "a.mkv", "manual.xml", 0.0)
            .unwrap();
        assert_eq!(
            store.load_bound_danmaku_path(1, "a.mkv").unwrap(),
            Some("manual.xml".into())
        );
        // 其他视频不受影响
        assert_eq!(
            store.load_bound_danmaku_path(1, "b.mkv").unwrap(),
            None
        );
    }

    #[test]
    fn remove_source_cascades_danmaku_bindings() {
        let store = memory_store();
        let sid = store.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint) VALUES('local', 's', 'D:\\s')",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        });
        store
            .upsert_danmaku_binding(sid, "a.mkv", "a.xml", -5.0)
            .unwrap();
        assert_eq!(
            store.load_danmaku_offset(sid, "a.mkv", "a.xml").unwrap(),
            Some(-5.0)
        );
        store.remove_source(sid).unwrap();
        assert_eq!(
            store.load_danmaku_offset(sid, "a.mkv", "a.xml").unwrap(),
            None
        );
        assert_eq!(store.load_bound_danmaku_path(sid, "a.mkv").unwrap(), None);
    }

    /// 轨道选择绑定：写 aid 不覆盖既有 sid（单列 upsert 契约），
    /// 关闭选择存 0，无行返回 None。
    #[test]
    fn track_binding_single_column_upsert() {
        use super::TrackKind;
        let store = memory_store();
        // 无行 → None。
        assert_eq!(store.load_track_binding(1, "v.mkv").unwrap(), None);
        // 首写 aid=2：另侧 sid 保持 NULL。
        store
            .save_track_selection(1, "v.mkv", TrackKind::Audio, Some(2))
            .unwrap();
        assert_eq!(
            store.load_track_binding(1, "v.mkv").unwrap(),
            Some((Some(2), None))
        );
        // 写 sid=3：aid 不被 NULL 覆盖。
        store
            .save_track_selection(1, "v.mkv", TrackKind::Subtitle, Some(3))
            .unwrap();
        assert_eq!(
            store.load_track_binding(1, "v.mkv").unwrap(),
            Some((Some(2), Some(3)))
        );
        // 显式关闭（None → 0）：只影响目标列。
        store
            .save_track_selection(1, "v.mkv", TrackKind::Subtitle, None)
            .unwrap();
        assert_eq!(
            store.load_track_binding(1, "v.mkv").unwrap(),
            Some((Some(2), Some(0)))
        );
        // aid 改选：sid 的 0 保留。
        store
            .save_track_selection(1, "v.mkv", TrackKind::Audio, Some(5))
            .unwrap();
        assert_eq!(
            store.load_track_binding(1, "v.mkv").unwrap(),
            Some((Some(5), Some(0)))
        );
        // 不同视频独立。
        assert_eq!(store.load_track_binding(1, "w.mkv").unwrap(), None);
    }

    /// remove_source 级联清理轨道绑定。
    #[test]
    fn remove_source_cascades_track_bindings() {
        use super::TrackKind;
        let store = memory_store();
        let sid = store.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sources(kind, name, endpoint) VALUES('local', 's', 'D:\\s')",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        });
        store
            .save_track_selection(sid, "a.mkv", TrackKind::Audio, Some(1))
            .unwrap();
        assert_eq!(
            store.load_track_binding(sid, "a.mkv").unwrap(),
            Some((Some(1), None))
        );
        store.remove_source(sid).unwrap();
        assert_eq!(store.load_track_binding(sid, "a.mkv").unwrap(), None);
    }
}

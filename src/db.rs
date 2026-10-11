use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Db(Mutex<Connection>);

pub fn db_path() -> PathBuf {
    crate::config::data_dir().join("siterip.db")
}

impl Db {
    pub fn open() -> Result<Self> {
        let p = db_path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let conn = Connection::open(&p)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS downloaded (
                vid TEXT PRIMARY KEY,
                site TEXT NOT NULL,
                title TEXT,
                file TEXT,
                ts INTEGER DEFAULT (strftime('%s','now'))
            )",
            [],
        )?;
        // 抓取结果持久化：程序重启后列表还在，无需重新抓取
        conn.execute(
            "CREATE TABLE IF NOT EXISTS entries (
                vid TEXT NOT NULL,
                site TEXT NOT NULL,
                title TEXT,
                url TEXT,
                thumb TEXT,
                ts INTEGER DEFAULT (strftime('%s','now')),
                PRIMARY KEY (vid, site)
            )",
            [],
        )?;
        // 工作项目（抓取会话）：每次抓取保存为一个可命名/恢复/删除的工作
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                site TEXT NOT NULL,
                ts INTEGER DEFAULT (strftime('%s','now'))
            )",
            [],
        )?;
        // 旧库迁移：给 entries 补 session_id 列（已存在则忽略报错）
        let _ = conn.execute("ALTER TABLE entries ADD COLUMN session_id INTEGER", []);
        // 工作项目 ⇄ 条目 映射表：同一视频可同时属于多个工作项目（抓取/手动保存互不覆盖）
        conn.execute(
            "CREATE TABLE IF NOT EXISTS session_items (
                sid INTEGER NOT NULL,
                vid TEXT NOT NULL,
                site TEXT NOT NULL,
                PRIMARY KEY (sid, vid, site)
            )",
            [],
        )?;
        // 旧数据迁移：把 entries.session_id 里的归属搬进映射表
        let _ = conn.execute(
            "INSERT OR IGNORE INTO session_items (sid, vid, site)
             SELECT session_id, vid, site FROM entries WHERE session_id IS NOT NULL",
            [],
        );
        Ok(Db(Mutex::new(conn)))
    }

    /// 创建工作项目，返回 id
    pub fn create_session(&self, name: &str, site: &str) -> Result<i64> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (name, site) VALUES (?1, ?2)",
            rusqlite::params![name, site],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn rename_session(&self, id: i64, name: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE sessions SET name=?1 WHERE id=?2",
            rusqlite::params![name, id],
        )?;
        Ok(())
    }

    /// 删除工作项目（只删项目和映射关系，不影响条目全局表与 downloaded 去重记录）
    pub fn delete_session(&self, id: i64) -> Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute("DELETE FROM session_items WHERE sid=?1", rusqlite::params![id])?;
        conn.execute("DELETE FROM sessions WHERE id=?1", rusqlite::params![id])?;
        Ok(())
    }

    /// 工作项目列表（含每项条目数），按创建时间倒序
    pub fn list_sessions(&self) -> Result<Vec<(i64, String, String, i64)>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.name, s.site, COUNT(si.vid)
             FROM sessions s LEFT JOIN session_items si ON si.sid = s.id
             GROUP BY s.id ORDER BY s.ts DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r?);
        }
        Ok(v)
    }

    /// 保存/更新一条抓取结果：条目全局去重 + 归属到指定工作项目（映射，不抢占）
    pub fn save_entry(&self, e: &crate::site::VideoEntry, session_id: i64) -> Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO entries (vid,site,title,url,thumb,session_id) VALUES (?1,?2,?3,?4,?5,?6)",
            rusqlite::params![e.id, e.site, e.title, e.url, e.thumb, session_id],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO session_items (sid, vid, site) VALUES (?1,?2,?3)",
            rusqlite::params![session_id, e.id, e.site],
        )?;
        Ok(())
    }

    /// 手动保存：事务内批量写入（几百条也是瞬间完成）
    pub fn save_session_bulk(&self, name: &str, site: &str, entries: &[crate::site::VideoEntry]) -> Result<i64> {
        let sid = self.create_session(name, site)?;
        let conn = self.0.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        for e in entries {
            tx.execute(
                "INSERT OR REPLACE INTO entries (vid,site,title,url,thumb,session_id) VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![e.id, e.site, e.title, e.url, e.thumb, sid],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO session_items (sid, vid, site) VALUES (?1,?2,?3)",
                rusqlite::params![sid, e.id, e.site],
            )?;
        }
        tx.commit()?;
        Ok(sid)
    }

    /// 更新已有工作项目的条目（整体替换映射）。配合手动保存的"幂等"语义：
    /// 同一个工作项目点多少次保存都只有一个会话，不再每次点都蹦出一个新的
    pub fn replace_session_items(&self, sid: i64, entries: &[crate::site::VideoEntry]) -> Result<()> {
        let conn = self.0.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM session_items WHERE sid=?1", rusqlite::params![sid])?;
        for e in entries {
            tx.execute(
                "INSERT OR REPLACE INTO entries (vid,site,title,url,thumb,session_id) VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![e.id, e.site, e.title, e.url, e.thumb, sid],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO session_items (sid, vid, site) VALUES (?1,?2,?3)",
                rusqlite::params![sid, e.id, e.site],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 读取某工作项目的全部条目（按抓取时间倒序）
    pub fn load_session_entries(&self, session_id: i64) -> Result<Vec<crate::site::VideoEntry>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT e.vid, e.site, e.title, e.url, e.thumb
             FROM entries e JOIN session_items si ON si.vid = e.vid AND si.site = e.site
             WHERE si.sid = ?1 ORDER BY e.ts DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |r| {
            Ok(crate::site::VideoEntry {
                site: r.get(1)?,
                id: r.get(0)?,
                title: r.get(2)?,
                url: r.get(3)?,
                thumb: r.get(4)?,
            })
        })?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r?);
        }
        Ok(v)
    }

    /// 启动恢复：某站点最近一个工作项目的条目；没有项目时回退到该站点全部条目
    pub fn load_entries(&self, site: &str) -> Result<Vec<crate::site::VideoEntry>> {
        let latest: Option<i64> = {
            let conn = self.0.lock().unwrap();
            conn.query_row(
                "SELECT id FROM sessions WHERE site=?1 ORDER BY ts DESC LIMIT 1",
                rusqlite::params![site],
                |r| r.get(0),
            )
            .ok()
        };
        if let Some(sid) = latest {
            return self.load_session_entries(sid);
        }
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT vid, title, url, thumb FROM entries WHERE site=?1 ORDER BY ts DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![site], |r| {
            Ok(crate::site::VideoEntry {
                site: site.to_string(),
                id: r.get::<_, String>(0)?,
                title: r.get::<_, String>(1)?,
                url: r.get::<_, String>(2)?,
                thumb: r.get::<_, Option<String>>(3)?,
            })
        })?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r?);
        }
        Ok(v)
    }

    pub fn has(&self, vid: &str, site: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT 1 FROM downloaded WHERE vid=?1 AND site=?2",
                rusqlite::params![vid, site],
                |_| Ok(()),
            )
            .is_ok()
    }

    pub fn mark(&self, vid: &str, site: &str, title: &str, file: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT OR REPLACE INTO downloaded (vid,site,title,file) VALUES (?1,?2,?3,?4)",
            rusqlite::params![vid, site, title, file],
        )?;
        Ok(())
    }

    pub fn remove(&self, vid: &str, site: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "DELETE FROM downloaded WHERE vid=?1 AND site=?2",
            rusqlite::params![vid, site],
        )?;
        Ok(())
    }
}

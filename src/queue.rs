use crate::config::Config;
use crate::db::Db;
use crate::site::VideoEntry;
use crate::ytdlp;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

#[derive(Serialize, Clone)]
pub struct JobUpdate {
    pub id: String,
    pub title: String,
    /// queued | running | done | failed | canceled
    pub state: String,
    pub pct: f64,
    pub msg: String,
    /// 实时下载速度（如 2.5MiB/s），无进度行时为空
    #[serde(default)]
    pub speed: String,
    /// 预计剩余时间（如 00:31）
    #[serde(default)]
    pub eta: String,
}

pub struct Queue {
    pub cancel: AtomicBool,
    pub active: Mutex<HashMap<String, JobUpdate>>,
    /// 被暂停的任务 id：排队中的任务挂起等待，恢复后继续
    pub paused: Mutex<std::collections::HashSet<String>>,
    /// 正在下载的任务的暂停标志（按 id）：置位后杀掉 yt-dlp，保留 .part
    pub pausing: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// 并发上限（可动态调整）与当前并发数。
    /// 不用固定容量的 Semaphore：那样并发数在整批开始时就焊死了，
    /// 中途改设置永远不生效；改小后正在跑的任务也不会被杀，跑完自然收敛。
    pub max_concurrent: AtomicUsize,
    pub running: AtomicUsize,
    /// 批次进行中追加进来的任务（再次点"下载选中"时并入当前队列，
    /// 而不是另起一个队列——旧队列还在后台跑，但轮询只看新队列，
    /// 进度会"消失"，同一个视频还会被两个队列同时下载）
    pub pending: Mutex<Vec<VideoEntry>>,
    /// 队列收尾循环退出后置位；之后再来批次才另起新队列
    pub ended: AtomicBool,
    /// 未收尾的任务数（tauri 的 JoinHandle 没有 is_finished，用计数器代替）
    pub workers: AtomicUsize,
}

impl Queue {
    /// 暂停任务：排队中的直接标记，下载中的置暂停标志（由下载循环杀进程）
    pub fn pause(&self, id: &str) {
        self.paused.lock().unwrap().insert(id.to_string());
        if let Some(f) = self.pausing.lock().unwrap().get(id) {
            f.store(true, Ordering::SeqCst);
        }
    }

    /// 恢复任务：清掉暂停标记和运行中的暂停标志，下载循环会重新拉起 yt-dlp
    pub fn resume(&self, id: &str) {
        self.paused.lock().unwrap().remove(id);
        if let Some(f) = self.pausing.lock().unwrap().get(id) {
            f.store(false, Ordering::SeqCst);
        }
    }

    /// 动态调整并发上限，立即对当前队列生效
    pub fn set_concurrency(&self, n: usize) {
        self.max_concurrent.store(n.max(1), Ordering::SeqCst);
    }

    /// 当前批次还在跑（或收尾循环还活着）吗
    pub fn is_busy(&self) -> bool {
        !self.ended.load(Ordering::SeqCst)
    }

    /// 把新任务并入正在跑的队列：跳过还在队列里/正在下载的，
    /// 已完成或失败的允许再次进来（配合前端清记录实现重下）
    pub fn add_pending(&self, entries: Vec<VideoEntry>) {
        let mut pending = self.pending.lock().unwrap();
        let active = self.active.lock().unwrap();
        for e in entries {
            if let Some(j) = active.get(&e.id) {
                if !["done", "failed", "canceled"].contains(&j.state.as_str()) {
                    continue; // 已排队/下载中/暂停的不要重复入队
                }
            }
            if pending.iter().any(|x| x.id == e.id) {
                continue;
            }
            pending.push(e);
        }
    }
}

static QUEUE: std::sync::Mutex<Option<Arc<Queue>>> = std::sync::Mutex::new(None);

pub fn current() -> Option<Arc<Queue>> {
    QUEUE.lock().unwrap().clone()
}

pub fn spawn(app: AppHandle, entries: Vec<VideoEntry>, cfg: Config) {
    let q = Arc::new(Queue {
        cancel: AtomicBool::new(false),
        active: Mutex::new(HashMap::new()),
        paused: Mutex::new(std::collections::HashSet::new()),
        pausing: Mutex::new(HashMap::new()),
        max_concurrent: AtomicUsize::new(cfg.concurrency.max(1) as usize),
        running: AtomicUsize::new(0),
        pending: Mutex::new(Vec::new()),
        ended: AtomicBool::new(false),
        workers: AtomicUsize::new(0),
    });
    // 必须覆盖而不是只设一次：OnceLock.set 第二次会静默失败，之后 get_jobs/
    // 暂停/取消全部还指着上一批已经跑完冻结的旧队列，第二批下载就成了"没反应"
    *QUEUE.lock().unwrap() = Some(q.clone());

    let total = entries.len();
    let finished = Arc::new(AtomicUsize::new(0));

    tauri::async_runtime::spawn(async move {
        let database = match Db::open() {
            Ok(d) => Arc::new(d),
            Err(e) => {
                let _ = app.emit("queue-error", e.to_string());
                if let Some(q) = QUEUE.lock().unwrap().as_ref() {
                    q.ended.store(true, Ordering::SeqCst);
                }
                let _ = app.emit("queue-done", 0usize);
                return;
            }
        };
        let mut remaining = entries;
        loop {
            if q.cancel.load(Ordering::SeqCst) {
                break;
            }
            // 收编批次进行中新提交的任务
            let add: Vec<VideoEntry> = q.pending.lock().unwrap().drain(..).collect();
            remaining.extend(add);
            for e in remaining.drain(..) {
                if database.has(&e.id, &e.site) {
                    finished.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
                let (app2, q2, db2, cfg2, fin2, qc) = (app.clone(), q.clone(), database.clone(), cfg.clone(), finished.clone(), q.clone());
                q.workers.fetch_add(1, Ordering::SeqCst);
                tauri::async_runtime::spawn(async move {
                    run_job(app2, q2, db2, cfg2, e, fin2, total).await;
                    qc.workers.fetch_sub(1, Ordering::SeqCst);
                });
            }
            // 全下完且没有新追加的才结束本批次
            if q.workers.load(Ordering::SeqCst) == 0 && q.pending.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        q.ended.store(true, Ordering::SeqCst);
        let _ = app.emit("queue-done", finished.load(Ordering::SeqCst));
    });
}

async fn run_job(
    app: AppHandle,
    q: Arc<Queue>,
    database: Arc<Db>,
    cfg: Config,
    entry: VideoEntry,
    finished: Arc<AtomicUsize>,
    total: usize,
) {
    let mut st = JobUpdate {
        id: entry.id.clone(),
        title: entry.title.clone(),
        state: "queued".into(),
        pct: 0.0,
        msg: String::new(),
        speed: String::new(),
        eta: String::new(),
    };
    let _ = app.emit("job-update", st.clone());
    q.active.lock().unwrap().insert(entry.id.clone(), st.clone());

    // 暂停检查：被暂停的任务在这里挂起（不占并发槽位），恢复或取消后继续
    loop {
        if q.cancel.load(Ordering::SeqCst) {
            st.state = "canceled".into();
            q.active.lock().unwrap().insert(entry.id.clone(), st.clone());
            let _ = app.emit("job-update", st);
            return;
        }
        if !q.paused.lock().unwrap().contains(&entry.id) {
            break;
        }
        if st.state != "paused" {
            st.state = "paused".into();
            q.active.lock().unwrap().insert(entry.id.clone(), st.clone());
            let _ = app.emit("job-update", st.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // 等并发槽位：CAS 抢占 running 计数，防多任务同时检查时超卖。
    // max_concurrent 是动态读的——用户中途改设置，下一批槽位就按新值放行
    loop {
        if q.cancel.load(Ordering::SeqCst) {
            st.state = "canceled".into();
            q.active.lock().unwrap().insert(entry.id.clone(), st.clone());
            let _ = app.emit("job-update", st);
            return;
        }
        let cur = q.running.load(Ordering::SeqCst);
        let max = q.max_concurrent.load(Ordering::SeqCst);
        if cur < max
            && q
                .running
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    st.state = "running".into();
    st.speed.clear();
    st.eta.clear();
    let _ = app.emit("job-update", st.clone());
    q.active.lock().unwrap().insert(entry.id.clone(), st.clone());

    let opts = ytdlp::DownloadOptions {
        proxy: cfg.proxy_for(&entry.site).map(|s| s.to_string()),
        quality: cfg.quality,
        out_dir: cfg.download_dir.clone(),
        cookies_txt: crate::cookies::cookies_txt_path(),
        site: entry.site.clone(),
    };

    // 每个任务一个暂停标志：pause 时杀掉 yt-dlp（.part 保留），恢复后重跑续传。
    // 外层 loop 就是"暂停→挂起→恢复→续传"的循环，正常完成/失败/取消才退出。
    let pause_flag = Arc::new(AtomicBool::new(false));
    q.pausing.lock().unwrap().insert(entry.id.clone(), pause_flag.clone());
    let result = loop {
        let flag = pause_flag.clone();
        let (app2, q2, job_id, mut st2) = (app.clone(), q.clone(), entry.id.clone(), st.clone());
        let result = ytdlp::download_video(&entry.url, &opts, flag, move |line| {
            if let Some(rest) = line.strip_prefix("[download]") {
                // "  42.3% of ~ 100.5MiB at 2.5MiB/s ETA 00:31"
                let s = rest.trim_start();
                if let Some(path) = s.strip_prefix("Destination:") {
                    st2.msg = path.trim().to_string();
                } else if let Some(pi) = s.find('%') {
                    // 百分比是 '%' 前的最后一个词（行尾跟的是 of/at/ETA，不能判 ends_with）
                    if let Ok(p) = s[..pi].rsplit(' ').next().unwrap_or("").trim().parse::<f64>() {
                        st2.pct = p;
                    }
                }
                if let Some(at) = s.find(" at ") {
                    let tail = &s[at + 4..];
                    let end = tail.find(" ETA").unwrap_or(tail.len());
                    st2.speed = tail[..end].trim().to_string();
                }
                if let Some(ei) = s.find("ETA ") {
                    st2.eta = s[ei + 4..].trim().to_string();
                }
                // 关键：同步更新活跃任务表——前端靠轮询 get_jobs 拿进度（事件通道不可靠）
                q2.active.lock().unwrap().insert(job_id.clone(), st2.clone());
                let _ = app2.emit("job-update", st2.clone());
            } else if line.starts_with("[Merger]") || line.starts_with("[ExtractAudio]") {
                st2.msg = line.clone();
                q2.active.lock().unwrap().insert(job_id.clone(), st2.clone());
                let _ = app2.emit("job-update", st2.clone());
            }
        })
        .await;

        if !pause_flag.load(Ordering::SeqCst) {
            break result;
        }

        // 是暂停不是失败：把进度留在这条任务上，挂起等恢复（或取消）
        st.state = "paused".into();
        st.speed.clear();
        st.eta.clear();
        let _ = app.emit("job-update", st.clone());
        q.active.lock().unwrap().insert(entry.id.clone(), st.clone());
        loop {
            if q.cancel.load(Ordering::SeqCst) {
                break;
            }
            if !q.paused.lock().unwrap().contains(&entry.id) && !pause_flag.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        if q.cancel.load(Ordering::SeqCst) {
            pause_flag.store(false, Ordering::SeqCst);
            break Err(anyhow::anyhow!("canceled"));
        }
        // 恢复了：清标志、回到 running，重跑 yt-dlp（自动从 .part 续传）
        pause_flag.store(false, Ordering::SeqCst);
        st.state = "running".into();
        let _ = app.emit("job-update", st.clone());
        q.active.lock().unwrap().insert(entry.id.clone(), st.clone());
    };

    q.pausing.lock().unwrap().remove(&entry.id);
    // st2 已被闭包吃掉，进度从活跃任务表里捞回来（暂停/续传后 UI 不清零）
    if let Some(s) = q.active.lock().unwrap().get(&entry.id) {
        st.pct = s.pct;
        st.msg = s.msg.clone();
    }
    // 释放并发槽位：下一条排队任务 200ms 内就会补上
    q.running.fetch_sub(1, Ordering::SeqCst);
    st.state = match &result {
        Ok(_) => "done".into(),
        Err(_) if q.cancel.load(Ordering::SeqCst) => "canceled".into(),
        Err(e) => {
            st.msg = e.to_string();
            "failed".into()
        }
    };
    if st.state == "done" {
        let _ = database.mark(&entry.id, &entry.site, &entry.title, &st.msg);
        finished.fetch_add(1, Ordering::SeqCst);
    }
    q.active.lock().unwrap().insert(entry.id.clone(), st.clone());
    let _ = app.emit("job-update", st);
    let _ = total;
}

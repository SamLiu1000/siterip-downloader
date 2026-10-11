mod auth;
mod config;
mod cookies;
mod db;
mod http;
mod queue;
mod scrape;
mod site;
mod ytdlp;

use anyhow::Result;
use serde::Serialize;
use std::os::windows::process::CommandExt;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Serialize, Clone)]
struct AppState {
    logged_in: bool,
    ytdlp_version: Option<String>,
    config: config::Config,
}

#[tauri::command]
fn get_state() -> Result<AppState, String> {
    crate::auth::log_debug("get_state: enter");
    let st = state_now();
    crate::auth::log_debug("get_state: exit");
    Ok(st)
}

fn state_now() -> AppState {
    AppState {
        logged_in: cookies::has_login(),
        ytdlp_version: ytdlp::cached_version(),
        config: config::Config::load().unwrap_or_default(),
    }
}

#[tauri::command]
fn save_config(cfg: config::Config) -> Result<(), String> {
    cfg.save().map_err(|e| e.to_string())
}

#[derive(Serialize, Clone)]
struct SiteInfo {
    id: String,
    display: String,
    home_url: String,
    favorites_url: String,
}

#[tauri::command]
fn list_sites() -> Result<Vec<SiteInfo>, String> {
    let cfg = config::Config::load().unwrap_or_default();
    Ok(site::all_adapters()
        .into_iter()
        .map(|a| SiteInfo {
            id: a.id().into(),
            display: a.display().into(),
            home_url: a.home_url().into(),
            // 用户配置优先；未配置（或被清空的串站数据）回落到站点默认收藏页
            favorites_url: cfg
                .favorites_urls
                .get(a.id())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| a.default_favorites_url().to_string()),
        })
        .collect())
}

#[tauri::command]
fn open_login(app: AppHandle, site: String, cfg: config::Config) -> Result<(), String> {
    auth::open_login(&app, &cfg, &site).map_err(|e| e.to_string())
}

/// 登录窗口工具栏的导航操作：back/forward/reload/home/goto
#[tauri::command]
fn nav_cmd(
    app: AppHandle,
    site: String,
    action: String,
    url: Option<String>,
) -> Result<(), String> {
    let label = auth::content_label(&site).ok_or("登录窗口未打开")?;
    let wv = app.get_webview(&label).ok_or("登录窗口已关闭")?;
    let res = match action.as_str() {
        "back" => wv.eval("history.back()"),
        "forward" => wv.eval("history.forward()"),
        "reload" => wv.eval("location.reload()"),
        "home" => {
            let home = site::adapter_for(&site)
                .map_err(|e| e.to_string())?
                .home_url()
                .to_string();
            let u: tauri::Url = match home.parse() {
                Ok(u) => u,
                Err(e) => return Err(e.to_string()),
            };
            wv.navigate(u)
        }
        "goto" => {
            let mut u = url.unwrap_or_default().trim().to_string();
            if !u.is_empty() && !u.starts_with("http") {
                u = format!("https://{u}");
            }
            wv.navigate(u.parse().map_err(|_| "地址无效")?)
        }
        other => return Err(format!("未知操作: {other}")),
    };
    res.map_err(|e| e.to_string())
}

/// 读取内容页当前 URL（工具栏轮询显示）
#[tauri::command]
fn nav_url(app: AppHandle, site: String) -> Result<String, String> {
    let label = auth::content_label(&site).ok_or("登录窗口未打开")?;
    app.get_webview(&label)
        .ok_or("登录窗口已关闭")?
        .url()
        .map(|u| u.to_string())
        .map_err(|e| e.to_string())
}

#[derive(Serialize, Clone)]
struct EntryStatus {
    #[serde(flatten)]
    entry: site::VideoEntry,
    downloaded: bool,
}

/// 扫描下载目录，提取文件名里 "[id]" 形式的视频 id 集合。
/// 已下载判定 = 数据库记录 || 磁盘上存在同名文件（防：手动挪动/记录丢失导致误报未下载）
fn downloaded_ids_in_dir(dir: &std::path::Path) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    // 文件可能直接在根目录（旧版布局），也可能在 <站点>/ 子目录里，两层都扫
    let mut dirs = vec![dir.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                dirs.push(e.path());
            }
        }
    }
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            if e.path().is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if let (Some(a), Some(b)) = (name.find('['), name.find(']')) {
                if b > a {
                    set.insert(name[a + 1..b].to_string());
                }
            }
        }
    }
    set
}

/// 判定某条目是否已下载（数据库记录 || 磁盘同名文件）
/// id 形态差异：抓取条目是 "video.uicefdfe8ba"（URL 前缀），yt-dlp 文件名是 "uicefdfe8ba"（纯 id），
/// 所以取 id 的最后一段再比对一次
fn is_downloaded(database: &db::Db, ids_on_disk: &std::collections::HashSet<String>, vid: &str, site: &str) -> bool {
    if database.has(vid, site) || ids_on_disk.contains(vid) {
        return true;
    }
    match vid.split('.').next_back() {
        Some(short) if short != vid => ids_on_disk.contains(short),
        _ => false,
    }
}

/// 拉取结果缓冲：前端轮询读取（事件通道在此环境不可靠）
static FETCHED: std::sync::LazyLock<std::sync::Mutex<Vec<EntryStatus>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));
static FETCH_DONE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[tauri::command]
fn get_favorites_so_far() -> Result<(Vec<EntryStatus>, bool), String> {
    let items = FETCHED.lock().unwrap().clone();
    let done = FETCH_DONE.load(std::sync::atomic::Ordering::SeqCst) == 1;
    Ok((items, done))
}

/// 流式拉取收藏列表：创建一个工作项目并落库，返回项目 id；每页 emit "fav-page"
#[tauri::command]
async fn fetch_favorites(app: AppHandle, site: String, cfg: config::Config) -> Result<i64, String> {
    let url = cfg
        .favorites_urls
        .get(&site)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(Ok)
        .unwrap_or_else(|| {
            // 未配置时回落到站点默认收藏页（如 X 站的 videos-i-like）
            match site::adapter_for(&site) {
                Ok(a) if !a.default_favorites_url().is_empty() => {
                    Ok(a.default_favorites_url().to_string())
                }
                _ => Err("请先在设置中填写该站点的收藏页地址".to_string()),
            }
        })?;
    // 地址归属自动纠正：地址栏内容是"当前选中站点"的槽位，用户常在选错站点时
    // 粘贴了别家站点的地址。按域名匹配真实归属站点，选错也能拉对。
    let mut site = site;
    let mut cfg = cfg;
    let url_host = tauri::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()));
    if let Some(host) = &url_host {
        let match_site = site::all_adapters()
            .into_iter()
            .find(|a| {
                let d = a.cookie_domain();
                host == d || host.ends_with(&format!(".{d}"))
            })
            .map(|a| a.id().to_string());
        match match_site {
            Some(real) if real != site => {
                crate::auth::log_debug(&format!(
                    "fetch_favorites: url belongs to '{real}', overriding requested site '{site}'"
                ));
                // 把地址搬到正确站点的槽位并持久化，界面上各站地址互不串扰
                cfg.favorites_urls.insert(real.clone(), url.clone());
                cfg.save().ok();
                site = real;
            }
            None => {
                return Err(format!(
                    "地址 {host} 不属于任何已支持的站点，请检查是否粘贴正确"
                ));
            }
            _ => {}
        }
    }
    crate::auth::log_debug(&format!("fetch_favorites: enter site={site}"));
    let adapter = site::adapter_for(&site).map_err(|e| e.to_string())?;

    // 渲染式抓取需要内容视图：没有就静默创建隐藏窗口（不弹窗、不打扰用户），
    // 再等 Cookie 回填就绪，避免恢复 Cookie 与抓取竞态
    auth::ensure_hidden(&app, &site).map_err(|e| e.to_string())?;
    let _ = auth::wait_ready(&site, std::time::Duration::from_secs(30));

    crate::auth::log_debug("fetch_favorites: db opened");
    FETCHED.lock().unwrap().clear();
    FETCH_DONE.store(0, std::sync::atomic::Ordering::SeqCst);
    let database = std::sync::Arc::new(db::Db::open().map_err(|e| e.to_string())?);
    let disk_ids = downloaded_ids_in_dir(&cfg.download_dir);
    let app_page = app.clone();
    let site_page = site.clone();
    let on_page = move |page: Vec<site::VideoEntry>| {
        let items: Vec<EntryStatus> = page
            .into_iter()
            .map(|entry| EntryStatus {
                downloaded: is_downloaded(&database, &disk_ids, &entry.id, &site_page),
                entry,
            })
            .collect();
        FETCHED.lock().unwrap().extend(items.iter().cloned());
        let _ = app_page.emit("fav-page", &items);
    };
    // 渲染式抓取是阻塞循环，放阻塞线程池避免卡住异步运行时
    let app2 = app.clone();
    let res = tauri::async_runtime::spawn(async move {
        adapter.list_favorites(&app2, &cfg, &url, &on_page).await
    })
    .await
    .map_err(|e| e.to_string())?;
    match res {
        Ok(total) => {
            FETCH_DONE.store(1, std::sync::atomic::Ordering::SeqCst);
            let _ = app.emit("fav-done", total);
            // 不再自动创建工作项目：只有前端手动"存当前"才落库成会话
            Ok(-1)
        }
        Err(e) => {
            FETCH_DONE.store(1, std::sync::atomic::Ordering::SeqCst);
            let _ = app.emit("fav-error", e.to_string());
            Err(e.to_string())
        }
    }
}

/// 启动时加载上次会话持久化的抓取结果（免重新抓取；downloaded 状态用数据库+磁盘文件双重判定）
#[tauri::command]
fn load_entries(cfg: config::Config, site: String) -> Result<Vec<EntryStatus>, String> {
    let database = db::Db::open().map_err(|e| e.to_string())?;
    let disk_ids = downloaded_ids_in_dir(&cfg.download_dir);
    Ok(database
        .load_entries(&site)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|entry| EntryStatus {
            downloaded: is_downloaded(&database, &disk_ids, &entry.id, &site),
            entry,
        })
        .collect())
}

/// ---- 工作项目管理 ----

#[tauri::command]
fn list_sessions() -> Result<Vec<(i64, String, String, i64)>, String> {
    db::Db::open().and_then(|d| d.list_sessions()).map_err(|e| e.to_string())
}

#[tauri::command]
fn rename_session(id: i64, name: String) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("名称不能为空".into());
    }
    db::Db::open().and_then(|d| d.rename_session(id, name)).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_session(id: i64) -> Result<(), String> {
    db::Db::open().and_then(|d| d.delete_session(id)).map_err(|e| e.to_string())
}

/// 手动把当前列表存为一个工作项目。带 sid = 更新已有项目（幂等保存，
/// 点多少次也只有一个）；sid 为空 = 新建（与自动抓取保存互不覆盖）
#[tauri::command]
fn save_session(
    site: String,
    name: String,
    entries: Vec<site::VideoEntry>,
    sid: Option<i64>,
) -> Result<i64, String> {
    let database = db::Db::open().map_err(|e| e.to_string())?;
    let name = name.trim();
    match sid {
        Some(sid) if sid > 0 => {
            database.rename_session(sid, if name.is_empty() { "未命名工作" } else { name })
                .map_err(|e| e.to_string())?;
            database.replace_session_items(sid, &entries).map_err(|e| e.to_string())?;
            Ok(sid)
        }
        _ => database
            .save_session_bulk(if name.is_empty() { "未命名工作" } else { name }, &site, &entries)
            .map_err(|e| e.to_string()),
    }
}

/// 点击某个工作项目，恢复它的抓取内容
#[tauri::command]
fn load_session(cfg: config::Config, id: i64) -> Result<Vec<EntryStatus>, String> {
    let database = db::Db::open().map_err(|e| e.to_string())?;
    let disk_ids = downloaded_ids_in_dir(&cfg.download_dir);
    Ok(database
        .load_session_entries(id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|entry| EntryStatus {
            downloaded: is_downloaded(&database, &disk_ids, &entry.id, &entry.site),
            entry,
        })
        .collect())
}

#[tauri::command]
fn start_download(
    app: AppHandle,
    entries: Vec<site::VideoEntry>,
    cfg: config::Config,
) -> Result<(), String> {
    match queue::current() {
        // 当前批次还在跑：把新任务并进去。另起新队列的话，旧队列仍在后台
        // 下载，但 get_jobs 轮询只看新队列——进度会"消失"，同一视频还
        // 会被两个队列同时写同一个文件
        Some(q) if q.is_busy() => q.add_pending(entries),
        _ => queue::spawn(app, entries, cfg),
    }
    Ok(())
}

#[tauri::command]
fn cancel_download() -> Result<(), String> {
    if let Some(q) = queue::current() {
        q.cancel.store(true, Ordering::SeqCst);
        // 置位所有运行中任务的暂停标志：下载循环会杀掉 yt-dlp，
        // 否则"取消"只拦住还没开跑的，正在下载的会一路跑到底
        for f in q.pausing.lock().unwrap().values() {
            f.store(true, Ordering::SeqCst);
        }
    }
    Ok(())
}

/// 动态修改并发上限：对当前正在跑的队列也立即生效（改小不杀正在跑的，
/// 跑完后按新值放行；改大立刻多放出任务）
#[tauri::command]
fn set_concurrency(n: usize) -> Result<(), String> {
    if let Some(q) = queue::current() {
        q.set_concurrency(n);
    }
    Ok(())
}

/// 前端轮询任务状态（事件通道在此环境不可靠，进度改为主动拉取）
#[tauri::command]
fn get_jobs() -> Result<Vec<queue::JobUpdate>, String> {
    Ok(match queue::current() {
        Some(q) => q.active.lock().unwrap().values().cloned().collect(),
        None => Vec::new(),
    })
}

/// 暂停单个任务：排队中的挂起；正在下载的杀掉 yt-dlp（保留 .part），恢复后续传
#[tauri::command]
fn pause_job(id: String) -> Result<(), String> {
    if let Some(q) = queue::current() {
        q.pause(&id);
    }
    Ok(())
}

/// 恢复已暂停的任务：排队的直接开跑，被暂停杀掉的下载从 .part 断点续传
#[tauri::command]
fn resume_job(id: String) -> Result<(), String> {
    if let Some(q) = queue::current() {
        q.resume(&id);
    }
    Ok(())
}

#[tauri::command]
fn forget_video(vid: String, site: String) -> Result<(), String> {
    db::Db::open()
        .and_then(|d| d.remove(&vid, &site))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_download_dir(cfg: config::Config) -> Result<(), String> {
    std::fs::create_dir_all(&cfg.download_dir).map_err(|e| e.to_string())?;
    opener::open(&cfg.download_dir).map_err(|e| e.to_string())
}

/// 系统原生"选择文件夹"对话框，返回用户选中的路径（取消返回 None）
#[tauri::command]
async fn pick_download_dir() -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择下载目录")
            .pick_folder()
            .map(|p| p.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| e.to_string())
}

/// 下载队列完成后的动作：shutdown（60 秒缓冲，可 shutdown /a 取消）或 sleep
#[tauri::command]
fn after_done_action(action: String) -> Result<(), String> {
    const NO_WINDOW: u32 = 0x08000000;
    match action.as_str() {
        "shutdown" => std::process::Command::new("shutdown")
            .args(["/s", "/t", "60"])
            .creation_flags(NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "sleep" => std::process::Command::new("rundll32.exe")
            .args(["powrprof.dll,SetSuspendState", "0,1,0"])
            .creation_flags(NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string()),
        _ => Ok(()),
    }
}

#[tauri::command]
fn logout() -> Result<(), String> {
    let p = cookies::cookie_file();
    if p.exists() {
        std::fs::remove_file(&p).map_err(|e| e.to_string())?;
    }
    let t = cookies::cookies_txt_path();
    if t.exists() {
        std::fs::remove_file(&t).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn main() {
    // 全局 panic 钩子：所有崩溃写入诊断日志（含位置）
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown".into()
        };
        crate::auth::log_debug(&format!("PANIC at {loc}: {msg}"));
    }));
    // WebView2 专属用户数据目录：与其它应用隔离，且可自愈
    let wv_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::env::temp_dir())
        .join("siterip")
        .join("WebView2");
    let _ = std::fs::create_dir_all(&wv_dir);
    // 自愈：上一次实例若被强杀，其 msedgewebview2 子进程可能残留并锁住数据目录，
    // 导致下次启动白屏。启动前清掉引用本目录的孤儿进程。
    let kill_stale = "Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" | Where-Object { $_.CommandLine -like '*siterip*WebView2*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }".to_string();
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &kill_stale])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &wv_dir);

    // WebView2 全进程统一浏览器参数：代理在这里注入，所有窗口（主界面+登录页）
    // 共享同一环境。逐窗口传 additional_browser_args 会导致多 webview 环境冲突
    //（内容视图 IPC 失败白屏）。本机回环地址默认绕过代理，主界面不受影响。
    // 会话 cookie 落盘必须无条件开启，否则重启后登录态丢失；
    // 代理（若配置）附加在同一参数串里。所有窗口共享同一环境。
    let mut wv_args = String::from("--persistent-session-cookies");
    if let Ok(cfg) = config::Config::load() {
        if let Some(proxy) = cfg.proxy_for("main") {
            wv_args = format!("--proxy-server={proxy} {wv_args}");
        }
    }
    // 测试辅助：SITERIP_CDP_PORT=9222 时开放 DevTools 协议端口，供 UI 自动化
    // 驱动/检查主界面 DOM（正常使用不设置该变量，无任何影响）
    if let Ok(port) = std::env::var("SITERIP_CDP_PORT") {
        wv_args = format!("--remote-debugging-port={port} {wv_args}");
    }
    std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", wv_args);
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 已有实例在运行：激活主窗口（最小化要先还原，否则焦点给了也看不见）
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .setup(|app| {
            let builder = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("Siterip Downloader - 收藏下载器")
            .inner_size(1200.0, 820.0)
            .min_inner_size(900.0, 600.0);
            builder.build()?;
            // 主窗口关闭 = 用户用完程序，退出整个进程。默认行为是"所有窗口
            // 都关了才退出"，而拉取收藏会留下隐藏抓取窗口，导致关掉主窗口
            // 后进程驻留后台；下次双击 exe 只会触发 single_instance 激活一个
            // 已不存在的窗口——用户感觉"程序有时点不开"。
            let main_win = app.get_webview_window("main").expect("main window");
            let h = app.handle().clone();
            main_win.on_window_event(move |e| {
                if matches!(e, tauri::WindowEvent::CloseRequested { .. }) {
                    h.exit(0);
                }
            });
            ytdlp::init_version();
            // 测试辅助：SITERIP_TEST_LOGIN=1 时启动即打开登录窗口（正常使用无影响）
            // 站点由 SITERIP_TEST_SITE 指定（默认 xvideos）；
            // =2 时先建隐藏抓取窗口再点"登录"——验证隐藏窗口被正确显示（弹窗回归测试）
            if matches!(
                std::env::var("SITERIP_TEST_LOGIN").as_deref(),
                Ok("1") | Ok("2")
            ) {
                let mode = std::env::var("SITERIP_TEST_LOGIN").unwrap_or_default();
                let site = std::env::var("SITERIP_TEST_SITE")
                    .unwrap_or_else(|_| "xvideos".to_string());
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let cfg = config::Config::load().unwrap_or_default();
                    if mode == "2" {
                        auth::log_debug("test: ensure_hidden first");
                        let _ = auth::ensure_hidden(&handle, &site);
                        std::thread::sleep(std::time::Duration::from_secs(5));
                    }
                    auth::log_debug("test: open_login now");
                    if let Err(e) = auth::open_login(&handle, &cfg, &site) {
                        eprintln!("[TEST] open_login failed: {e:#}");
                    }
                });
            }
            // 测试辅助：SITERIP_TEST_FETCH=1 时启动即执行一次收藏抓取并写日志
            if std::env::var("SITERIP_TEST_FETCH").as_deref() == Ok("1") {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                    let cfg = config::Config::load().unwrap_or_default();
                    let url = cfg
                        .favorites_urls
                        .get("xvideos")
                        .cloned()
                        .unwrap_or_default();
                    let _ = auth::open_login(&handle, &cfg, "xvideos");
                    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                    let h2 = handle.clone();
                    let on_page = move |page: Vec<site::VideoEntry>| {
                        auth::log_debug(&format!(
                            "fav-page: {} items, first={:?}",
                            page.len(),
                            page.first().map(|e| (e.id.clone(), e.title.clone()))
                        ));
                    };
                    let adapter = site::adapter_for("xvideos");
                    let res = match adapter {
                        Ok(a) => a.list_favorites(&h2, &cfg, &url, &on_page).await,
                        Err(e) => Err(e),
                    };
                    auth::log_debug(&format!(
                        "fetch result: {} items, err={:?}",
                        res.as_ref().copied().unwrap_or(0),
                        res.as_ref().err().map(|e| e.to_string())
                    ));
                });
            }
            // 测试辅助：验证"关闭登录窗口后再次打开"不白屏
            if std::env::var("SITERIP_TEST_REOPEN").as_deref() == Ok("1") {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    let sleep = std::time::Duration::from_secs;
                    std::thread::sleep(sleep(3));
                    let cfg = config::Config::load().unwrap_or_default();
                    let _ = auth::open_login(&handle, &cfg, "xvideos");
                    auth::log_debug("reopen test: first open done");
                    std::thread::sleep(sleep(8));
                    // 模拟用户关窗口：主线程销毁
                    let h2 = handle.clone();
                    let _ = handle.run_on_main_thread(move || {
                        if let Some(w) = h2.get_window("xvideos-login-0") {
                            w.close().ok();
                        }
                    });
                    auth::log_debug("reopen test: closed first window");
                    std::thread::sleep(sleep(4));
                    let _ = auth::open_login(&handle, &cfg, "xvideos");
                    auth::log_debug("reopen test: second open done");
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            list_sites,
            save_config,
            open_login,
            nav_cmd,
            nav_url,
            fetch_favorites,
            get_favorites_so_far,
            load_entries,
            list_sessions,
            rename_session,
            delete_session,
            load_session,
            save_session,
            start_download,
            cancel_download,
            set_concurrency,
            get_jobs,
            pause_job,
            resume_job,
            forget_video,
            open_download_dir,
            pick_download_dir,
            after_done_action,
            logout
        ])
        .run(tauri::generate_context!())
        .expect("error while running siterip");
}

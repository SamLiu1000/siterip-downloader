use crate::config::Config;
use crate::cookies::{self, Cookie};
use crate::site::adapter_for;
use anyhow::Result;
use cookie::Expiration as CookieExpiration;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl};

static OPEN_SEQ: AtomicU32 = AtomicU32::new(0);
static WINDOW_SITES: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CONTENT_LABEL: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// 登录窗口完成 Cookie 回填后的就绪标记
static READY: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// 已对各窗口"钉住"的登录 cookie（label -> name -> 上次钉住的值），
/// 值变化时重写，跟随服务端轮换
static PINNED: LazyLock<Mutex<HashMap<String, HashMap<String, String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// 每站点窗口创建互斥：防止"拉取(隐藏窗)"与"点登录"并发时双开窗口
static CREATE_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn site_lock(site: &str) -> Arc<Mutex<()>> {
    CREATE_LOCKS
        .lock()
        .unwrap()
        .entry(site.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// 站点首页主机名（如 www.xvideos.com）。WebView2 的 set_cookie 对带点
/// 域名（".xvideos.com"）会归一化成仅主机 cookie（host_key 无点前缀），
/// 永远不会发往 www 子域（实测数据库中此类 cookie last_access 恒为 0），
/// 因此所有注入一律使用首页主机名，保证请求必带。
fn site_host(home_url: &str) -> String {
    tauri::Url::parse(home_url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// 按 cookie 原有属性构建要注入的 cookie；expiry_override 非 None 时使用
/// 指定过期时间（钉住持久化用），否则沿用原过期时间
fn build_cookie(c: &cookies::Cookie, host: &str, expiry_override: Option<i64>) -> cookie::Cookie<'static> {
    let mut ck = cookie::Cookie::build((c.name.clone(), c.value.clone()))
        .domain(host.to_string())
        .path(if c.path.is_empty() { "/".to_string() } else { c.path.clone() });
    let exp = expiry_override.unwrap_or(c.expiry);
    if exp > 0 {
        if let Ok(dt) = time::OffsetDateTime::from_unix_timestamp(exp) {
            ck = ck.expires(dt);
        }
    }
    // 属性须与原 cookie 一致：Secure/httpOnly/SameSite 不一致时浏览器
    // 可能拒收或不随请求发送，注入等于没注入
    if c.secure {
        ck = ck.secure(true);
    }
    if c.http_only {
        ck = ck.http_only(true);
    }
    match c.same_site.as_str() {
        "strict" => ck = ck.same_site(cookie::SameSite::Strict),
        "none" => ck = ck.same_site(cookie::SameSite::None),
        "lax" => ck = ck.same_site(cookie::SameSite::Lax),
        _ => {}
    }
    ck.build()
}

pub fn wait_ready(site: &str, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if READY.lock().unwrap().get(site).copied().unwrap_or(false) {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// 当前某站点登录窗口的内容视图标签（工具栏导航、抓取等用）
pub fn content_label(site: &str) -> Option<String> {
    CONTENT_LABEL.lock().unwrap().get(site).cloned()
}

#[derive(Serialize, Clone)]
pub struct LoginOutcome {
    pub ok: bool,
    pub message: String,
}

/// 打开登录窗口：单窗口双 webview —— 顶部导航工具栏（本地页面）+ 下方站点内容。
/// WebView2 不走 reqwest 的代理，需显式传 --proxy-server，否则被墙站点加载空白。
/// 外部页面的弹窗（广告）没有处理器会直接被丢弃，等于自带防弹窗。
fn open_login_inner(app: &AppHandle, site: &str, visible: bool) -> Result<()> {
    let adapter = adapter_for(site)?;
    // 已有该站点的登录窗口则聚焦
    for (l, s) in WINDOW_SITES.lock().unwrap().iter() {
        if s == site {
            if let Some(w) = app.get_webview(l) {
                if !visible {
                    // 隐藏模式（拉取专用）：窗口已在就直接用，绝不打断/置前
                    return Ok(());
                }
                // 已存在：还原（取消最小化）、显示并置前，避免"点了没反应"
                let win = w.window();
                win.show().ok();
                win.unminimize().ok();
                win.set_focus().ok();
                return Ok(());
            }
        }
    }
    // 每次打开用全新的唯一标签：窗口关闭后旧标签不会立即释放，复用同名标签会失败
    let n = OPEN_SEQ.fetch_add(1, Ordering::SeqCst);
    let win_label = format!("{site}-login-{n}");
    WINDOW_SITES
        .lock()
        .unwrap()
        .insert(win_label.clone(), site.to_string());
    CONTENT_LABEL
        .lock()
        .unwrap()
        .insert(site.to_string(), win_label.clone());
    let poll_label = win_label.clone();

    let url: tauri::Url = adapter.home_url().parse()?;

    // 窗口必须在主线程创建：tao 的窗口跨线程创建会间歇性崩溃或得到不渲染的白窗口
    // 结构 = 外壳窗口 + 两个子 webview：顶部导航工具栏（本地页面，带地址栏）+
    // 下方站点内容。wl 继续作为内容 webview 的标签——nav_cmd / scrape / cookie
    // 轮询全靠它找内容页，语义不变。
    let app2 = app.clone();
    let wl = win_label.clone();
    let win_label = format!("{wl}-win");
    let bar_label = format!("{wl}-bar");
    const BAR_H: f64 = 46.0;
    let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();
    app.run_on_main_thread(move || {
        let res = (|| -> tauri::Result<()> {
            let win = {
                let mut b = tauri::window::WindowBuilder::new(&app2, &win_label)
                    .title(format!("登录 {} —— 登录成功后直接关闭此窗口", adapter.display()))
                    .inner_size(1150.0, 820.0);
                if !visible {
                    b = b.visible(false);
                }
                b.build()?
            };
            let bar = tauri::webview::WebviewBuilder::new(
                &bar_label,
                WebviewUrl::App("login-toolbar.html".into()),
            );
            win.add_child(
                bar,
                tauri::LogicalPosition::new(0.0, 0.0),
                tauri::LogicalSize::new(1150.0, BAR_H),
            )?;
            // OAuth 弹窗处理：站点"使用 Google/Apple/微软/X 登录"走 window.open，
            // WebView2 对新窗口请求默认丢弃（没有处理器时广告弹窗也因此不弹），
            // 后果是点 Google 登录毫无反应。这里挂上处理器：授权域名白名单内的
            // 弹窗导航到登录窗口自身完成（授权回调会写登录 Cookie，轮询照常
            // 捕获）；其余弹窗（广告）维持丢弃。
            let app3 = app2.clone();
            let wl3 = wl.clone();
            let content = tauri::webview::WebviewBuilder::new(&wl, WebviewUrl::External(url))
                .initialization_script(NET_HOOK_JS)
                .on_new_window(move |url, _features| {
                    let u = url.to_string();
                    const OAUTH_KEYS: &[&str] = &[
                        "accounts.google.com",
                        "accounts.video.google.com",
                        "/o/oauth2",
                        "appleid.apple.com",
                        "login.microsoftonline.com",
                        "login.live.com",
                        "twitter.com/i/flow/login",
                        "x.com/i/flow/login",
                    ];
                    let is_oauth = OAUTH_KEYS.iter().any(|k| u.contains(k));
                    log_debug(&format!(
                        "new-window request: {u} -> {}",
                        if is_oauth { "navigate self" } else { "deny" }
                    ));
                    if is_oauth {
                        if let Some(wv) = app3.get_webview(&wl3) {
                            let _ = wv.navigate(url.clone());
                        }
                    }
                    tauri::webview::NewWindowResponse::Deny
                });
            win.add_child(
                content,
                tauri::LogicalPosition::new(0.0, BAR_H),
                tauri::LogicalSize::new(1150.0, 820.0 - BAR_H),
            )?;
            // 显式抢前台：Windows 对非前台线程建窗有前台锁定策略，
            // 不主动 set_focus 时新窗口可能整个被主窗口挡住（任务栏闪橙），
            // 用户感觉"点了登录没弹窗"
            if visible {
                let _ = win.set_focus();
            }
            // 缩放窗口时同步两个 webview 的边界（多 webview 不会自动跟随布局）
            let win2 = win.clone();
            let wl2 = wl.clone();
            let bar2 = bar_label.clone();
            win.on_window_event(move |e| {
                if let tauri::WindowEvent::Resized(size) = e {
                    let scale = win2.scale_factor().unwrap_or(1.0);
                    let th = (BAR_H * scale).round() as i32;
                    let w = size.width as i32;
                    let h = size.height as i32;
                    if let Some(b) = win2.get_webview(&bar2) {
                        let _ = b.set_bounds(tauri::Rect {
                            position: tauri::PhysicalPosition::new(0, 0).into(),
                            size: tauri::PhysicalSize::new(w, th).into(),
                        });
                    }
                    if let Some(c) = win2.get_webview(&wl2) {
                        let _ = c.set_bounds(tauri::Rect {
                            position: tauri::PhysicalPosition::new(0, th).into(),
                            size: tauri::PhysicalSize::new(w, (h - th).max(1)).into(),
                        });
                    }
                }
            });
            Ok(())
        })();
        let _ = tx.send(res.map_err(anyhow::Error::from));
    })
    .map_err(|e| anyhow::anyhow!("主线程调度失败: {e}"))?;
    // WebView2 环境冷启动（首次创建浏览器进程、杀软扫描）可能远超 30 秒，
    // 超时太短会误报"创建登录窗口超时"且留下半创建状态
    rx.recv_timeout(std::time::Duration::from_secs(90))
        .map_err(|_| anyhow::anyhow!("创建登录窗口超时"))??;

    // 会话 Cookie（含 httpOnly 的 session_token_auth）不随进程持久化：
    // 用 WebView2 原生 set_cookie API 直接注入浏览器 Cookie 存储。
    // 关键坑（实测 Cookie 数据库证实）：domain 传 ".xvideos.com" 会被
    // WebView2 归一化成"仅主机"cookie（host_key 无点前缀），永远不发往
    // www 子域，恢复等于没恢复。所以一律注入到站点首页主机名
    // （www.xvideos.com）名下，保证请求必带。
    // 只补缺：浏览器里已有任意域的同名 cookie 就不注入 —— 站点的 session
    // token 会轮换，浏览器里的是最新值，用 cookies.json 里的旧值覆盖会
    // 直接把登录态作废。
    {
        let saved = cookies::load().unwrap_or_default();
        let wv = app.get_webview(&poll_label);
        if !saved.is_empty() && wv.is_some() {
            let wv = wv.unwrap();
            let host = site_host(adapter.home_url());
            let existing: std::collections::HashSet<String> = match wv.cookies() {
                Ok(raw) => raw.iter().map(|c| c.name().to_string()).collect(),
                Err(_) => Default::default(),
            };
            let mut n_ok = 0usize;
            for c in &saved {
                if c.name.starts_with("xv") || c.name.starts_with("srcc") {
                    continue; // 程序诊断/抓取回传 cookie，不注入
                }
                if existing.contains(&c.name) {
                    continue; // 浏览器已有（可能更新），不能拿旧值覆盖
                }
                if let Err(e) = wv.set_cookie(build_cookie(c, &host, None)) {
                    log_debug(&format!("set_cookie {} err: {e}", c.name));
                } else {
                    n_ok += 1;
                }
            }
            log_debug(&format!("native cookie restore: {n_ok}/{} injected", saved.len()));
            // Cookie 已入存储，刷新页面使其生效
            if n_ok > 0 {
                let _ = wv.eval("location.reload()");
            }
            READY.lock().unwrap().insert(site.to_string(), true);
        } else if !saved.is_empty() {
            READY.lock().unwrap().insert(site.to_string(), true);
        } else {
            // 无已存 Cookie（首次使用），无需回填
            READY.lock().unwrap().insert(site.to_string(), true);
        }
    }

    // Cookie 轮询保存 + 页面内工具栏补挂：每 3 秒在主线程执行一次。
    // 不拦截关闭事件，窗口点 × 自然关闭。
    let app2 = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(3));
        if app2.get_webview(&poll_label).is_none() {
            // 窗口已关闭：清掉映射，避免残留条目干扰后续的复用判断
            let site_key = poll_label.split('-').next().unwrap_or("").to_string();
            WINDOW_SITES.lock().unwrap().remove(&poll_label);
            let mut cl = CONTENT_LABEL.lock().unwrap();
            if cl.get(&site_key).map(String::as_str) == Some(poll_label.as_str()) {
                cl.remove(&site_key);
            }
            drop(cl);
            PINNED.lock().unwrap().remove(&poll_label);
            READY.lock().unwrap().remove(&site_key);
            break; // 窗口已关闭，结束轮询
        }
        let app3 = app2.clone();
        let cl = poll_label.clone();
        let _ = app2.run_on_main_thread(move || {
            poll_cookies(&app3, &cl);
        });
    });
    Ok(())
}

/// 对外入口：永远在后台线程创建窗口。
/// 主线程（命令处理）直接创建 webview 会间歇性得到不渲染的白窗口。
/// 注意不能在这里因"已有该站点的 webview"而提前返回：拉取收藏会创建
/// 隐藏窗口（ensure_hidden），提前返回会让用户点"登录"毫无反应——
/// 统一交给 open_login_inner，它会把已有窗口（含隐藏的）还原并置前。
pub fn open_login(app: &AppHandle, cfg: &Config, site: &str) -> Result<()> {
    let _ = cfg;
    let app = app.clone();
    let site = site.to_string();
    std::thread::spawn(move || {
        let lock = site_lock(&site);
        let _g = lock.lock().unwrap();
        if let Err(e) = open_login_inner(&app, &site, true) {
            log_debug(&format!("open_login error: {e:#}"));
            // 失败要可见：之前只写日志，用户点登录没反应也不知道原因
            let _ = app.emit(
                "login-done",
                LoginOutcome { ok: false, message: format!("打开登录窗口失败：{e:#}") },
            );
        }
    });
    Ok(())
}

/// 确保某站点存在一个可用的内容视图（拉取/渲染式抓取用），但不显示、不聚焦：
/// 已有窗口（无论可见与否）直接复用；没有就在后台创建一个隐藏窗口。
/// 这样"拉取"对用户零打扰——不会再弹出登录窗口。
pub fn ensure_hidden(app: &AppHandle, site: &str) -> Result<()> {
    let app = app.clone();
    let site = site.to_string();
    std::thread::spawn(move || {
        let lock = site_lock(&site);
        let _g = lock.lock().unwrap();
        if let Err(e) = open_login_inner(&app, &site, false) {
            log_debug(&format!("ensure_hidden error: {e:#}"));
        }
    });
    Ok(())
}


/// 文档开始时注入：钩住 fetch/XHR，把站点数据接口地址写入 cookie（诊断 + 发现 API）
pub const NET_HOOK_JS: &str = r#"
(function(){
  if (window.__xvhook2) return; window.__xvhook2 = 1;
  window.__xvurls = [];
  function rec(kind, u) {
    try {
      u = '' + u;
      if (u.indexOf('xvideos') < 0) return;
      if (/\.(js|png|jpg|gif|css|woff|ico|mp4)(\?|$)/.test(u)) return;
      window.__xvurls.push(kind + ' ' + u.slice(0, 200));
      document.cookie = 'xvurls=' + encodeURIComponent(window.__xvurls.join('|').slice(-3000)) + ';path=/';
    } catch(e) {}
  }
  var of = window.fetch;
  window.fetch = function(u, o) { rec('F', typeof u === 'string' ? u : (u && u.url)); return of.apply(this, arguments); };
  var oo = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function(m, u) { rec('X', u); return oo.apply(this, arguments); };
})();
"#;

/// 注入页面的悬浮导航条：返回/前进/刷新/回首页（纯 JS，页面跳转后由轮询自动补挂）
const TOOLBAR_JS: &str = r#"
(function(){
  if (document.getElementById('xvfav-bar')) return;
  var d = document.createElement('div');
  d.id = 'xvfav-bar';
  d.style.cssText = 'position:fixed;top:0;left:0;width:auto;z-index:2147483647;background:#1d1f26;border:1px solid #32363f;border-radius:0 0 8px 0;padding:5px;display:flex;gap:6px;font-family:sans-serif;';
  function mk(label, fn){
    var b = document.createElement('button');
    b.textContent = label;
    b.style.cssText = 'width:36px;height:30px;background:#262933;color:#fff;border:1px solid #32363f;border-radius:6px;cursor:pointer;font-size:14px;';
    b.onclick = fn;
    d.appendChild(b);
  }
  mk('◀', function(){ history.back(); });
  mk('▶', function(){ history.forward(); });
  mk('⟳', function(){ location.reload(); });
  mk('⌂', function(){ location.href = 'https://www.xvideos.com/'; });
  document.documentElement.appendChild(d);
})();
"#;

/// 在主线程执行：抓取内容视图的 Cookie 并保存
fn poll_cookies(app: &AppHandle, content_label: &str) {
    let site = content_label.split('-').next().unwrap_or("").to_string();
    let adapter = match adapter_for(&site) {
        Ok(a) => a,
        Err(_) => return,
    };
    let Some(wv) = app.get_webview(content_label) else {
        return;
    };
    // 补挂页面内导航工具栏（页面跳转后会丢失）
    let _ = wv.eval(TOOLBAR_JS);
    // 诊断：记录内容视图当前 URL
    let url_now = wv
        .url()
        .map(|u| u.to_string())
        .unwrap_or_else(|e| format!("url_err:{e}"));
    log_debug(&format!("content url: {url_now}"));
    let raw = match wv.cookies() {
        Ok(r) => r,
        Err(_) => return,
    };
    let list: Vec<Cookie> = raw
        .iter()
        .filter(|c| {
            c.domain()
                .map(|d| {
                    let d = d.trim_start_matches('.');
                    let dom = adapter.cookie_domain();
                    d == dom || d.ends_with(&format!(".{dom}"))
                })
                .unwrap_or(false)
        })
        .map(|c| Cookie {
            name: c.name().to_string(),
            value: c.value().to_string(),
            domain: c.domain().unwrap_or("").to_string(),
            path: c.path().unwrap_or("/").to_string(),
            expiry: match c.expires() {
                Some(CookieExpiration::DateTime(dt)) => dt.unix_timestamp(),
                _ => 0,
            },
            secure: c.secure().unwrap_or(false),
            http_only: c.http_only().unwrap_or(false),
            same_site: match c.same_site() {
                Some(cookie::SameSite::Strict) => "strict".into(),
                Some(cookie::SameSite::None) => "none".into(),
                Some(cookie::SameSite::Lax) => "lax".into(),
                _ => String::new(),
            },
        })
        .collect();
    if list.is_empty() {
        return;
    }
    // 页面侧诊断回报
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_DBG: AtomicU64 = AtomicU64::new(0);
    if let Some(d) = list.iter().find(|c| c.name == "xv_dbg") {
        let h = fxhash(d.value.as_bytes());
        if LAST_DBG.swap(h, Ordering::SeqCst) != h {
            crate::auth::log_debug(&format!("page dbg: {}", urldecode(&d.value)));
        }
    }
    let n = list.len();
    if cookies::save(&list).is_ok() {
        // 检测到登录态 Cookie 时通知主界面
        let logged = list
            .iter()
            .any(|c| c.name.contains("login") || c.name.contains("session") || c.name.contains("token"));
        if logged {
            let _ = app.emit("login-progress", LoginOutcome {
                ok: true,
                message: format!("已自动保存 {n} 条 Cookie（登录态已捕获）"),
            });
            // 把登录态 Cookie 以长有效期重写进浏览器（值不变，仅加过期时间）：
            // 登录 cookie 通常是无过期时间的会话 cookie，浏览器不落盘，进程
            // 一重启就丢 —— 这是"每次打开弹窗都要重新登录"的根因。重写后
            // WebView2 会把它持久化到磁盘，重启后登录直接还在。
            // 注入到首页主机名下（见 open_login_inner 的注释）；值变化时
            // 覆盖重写，保持与服务端轮换同步。
            let host = site_host(adapter.home_url());
            let mut cache = PINNED.lock().unwrap();
            let pinned = cache.entry(content_label.to_string()).or_default();
            let mut n_pin = 0usize;
            for c in &list {
                if !(c.name.contains("session") || c.name.contains("login") || c.name.contains("token")) {
                    continue;
                }
                if pinned.get(&c.name) == Some(&c.value) {
                    continue; // 未变化，无需重写
                }
                if wv.set_cookie(build_cookie(c, &host, Some(
                    time::OffsetDateTime::now_utc().unix_timestamp() + 180 * 86400,
                ))).is_ok() {
                    pinned.insert(c.name.clone(), c.value.clone());
                    n_pin += 1;
                }
            }
            if n_pin > 0 {
                log_debug(&format!("login cookies pinned to {host} with 180d expiry: {n_pin}"));
            }
        }
    }
}

/// 生成回填 Cookie 的 JS（跳过程序自身的诊断 cookie；非法值跳过）
fn restore_cookies_js() -> String {
    let Ok(list) = cookies::load() else { return String::new() };
    let mut js = String::from("(function(){");
    for c in list {
        if c.name.starts_with("xv") && (c.name.starts_with("xv_") || c.name.starts_with("xvc") ) {
            continue;
        }
        if c.domain.is_empty() || !c.domain.contains("xvideos") {
            continue;
        }
        // cookie 值必须不含分号/逗号/空白才能安全回填
        if c.value.is_empty()
            || c.value.chars().any(|ch| ch == ';' || ch == ',' || ch.is_whitespace())
        {
            continue;
        }
        let max_age = if c.expiry > 0 {
            c.expiry
                - std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
        } else {
            31536000
        }
        .max(3600);
        // 值经过 JSON 转义避免引号/反斜杠破坏 JS 字符串
        let vesc = serde_json::to_string(&c.value).unwrap_or_default();
        js.push_str(&format!(
            "document.cookie={}+'={}; domain=.xvideos.com; path=/; max-age={};';",
            serde_json::to_string(&c.name).unwrap_or_default(),
            vesc,
            max_age
        ));
    }
    js.push_str("})();");
    js
}

pub fn log_debug(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::cookies::debug_log_path())
    {
        let _ = writeln!(f, "[{}] {}", chrono_now(), msg);
    }
}

fn chrono_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

pub fn fxhash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut outv = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    outv.push(v);
                    i += 3;
                } else {
                    outv.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                outv.push(b' ');
                i += 1;
            }
            b => {
                outv.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&outv).to_string()
}

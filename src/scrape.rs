//! 渲染式抓取：站点列表页由 JS 动态加载（纯 HTTP 只能拿到空壳），
//! 因此用隐藏的 WebView2 视图渲染页面（共享主环境：代理 + 登录态），
//! 注入 JS 提取视频链接，数据经本机回环 HTTP（Image GET）回传，滚动触发懒加载。

use crate::site::VideoEntry;
use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::sync::mpsc;
use std::time::Duration;
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, serde::Deserialize)]
struct RawItem {
    id: String,
    #[serde(default)]
    u: String,
    #[serde(default)]
    t: String,
}

/// 注入页面的提取脚本：收集视频锚点 -> CORS POST 到本机收集器 -> 滚动触发懒加载。
/// https 页面向 http://127.0.0.1 发请求属私有网络访问，浏览器会先发预检，
/// 收集器必须回应 Access-Control-Allow-Private-Network: true。
const EXTRACT_JS: &str = r#"
(function(){
  // 钩住 fetch/XHR 记录数据接口地址（诊断用）
  try {
    if (!window.__xvhook) {
      window.__xvhook = 1;
      window.__xvurls = [];
      var of = window.fetch;
      window.fetch = function(u, o) {
        try { var s2 = typeof u === 'string' ? u : (u && u.url) || '';
          if (s2.indexOf('xvideos') >= 0 && s2.indexOf('.js') < 0 && s2.indexOf('.png') < 0 && s2.indexOf('.jpg') < 0) window.__xvurls.push('F ' + s2.slice(0, 160));
        } catch(e) {}
        return of.apply(this, arguments);
      };
      var oo = XMLHttpRequest.prototype.open;
      XMLHttpRequest.prototype.open = function(m, u) {
        try { if (('' + u).indexOf('xvideos') >= 0) window.__xvurls.push('X ' + ('' + u).slice(0, 160)); } catch(e) {}
        return oo.apply(this, arguments);
      };
    }
  } catch(e) {}
  // 自动关闭年龄确认/免责声明等全屏弹窗
  try {
    document.querySelectorAll('button, .btn, a.btn, [role=button], input[type=button], input[type=submit]').forEach(function(b){
      if (!b.offsetParent && !b.offsetWidth) return;
      var tx = (b.textContent || b.value || '').toLowerCase();
      if (/accept|agree|enter|i.?m 18|i am 18|over 18|continue|同意|进入|我已满/.test(tx)) { b.click(); }
    });
  } catch(e) {}
  var out = {};
  document.querySelectorAll('a[href]').forEach(function(a){
    var href = a.getAttribute('href') || '';
    var m = href.match(/(?:^|\/)(video(?:\.[a-z0-9]+|\d+))(?:\/([^\/?#]*))?/i);
    if (m && !out[m[1]]) {
      var t = (a.getAttribute('title') || a.textContent || '').trim().slice(0, 140);
      out[m[1]] = [m[1], m[2] || '', t];
    }
  });
  var hrefs = [];
  document.querySelectorAll('a[href]').forEach(function(a){ hrefs.push(a.getAttribute('href')); });
  var vhrefs = hrefs.filter(function(h){ return (h||'').indexOf('video') >= 0; }).slice(0, 6);
  try {
    var g = document.querySelector('#page-videos-content');
    var gl = g ? g.querySelectorAll('a[href]') : [];
    var hs = [];
    for (var i = 0; i < gl.length && i < 6; i++) hs.push(gl[i].getAttribute('href'));
    var xu = (window.__xvurls || []).slice(0, 5).join(' ; ');
    var authc = document.cookie.indexOf('session_token_auth') >= 0;
    var hook = typeof window.__xvhook2 !== 'undefined';
    var dbg = 'auth=' + authc + '|hook=' + hook + '|XHR=' + encodeURIComponent(xu) + '|gridN=' + (g ? g.querySelectorAll('a').length : -1) +
      '|links=' + encodeURIComponent(hs.join(' ; ').slice(0, 800)) +
      '|ghtml=' + encodeURIComponent((g ? g.innerHTML : '').slice(0, 400));
    document.cookie = 'xv_dbg=' + dbg + ';path=/';
  } catch(e) {}
  try {
    document.querySelectorAll('iframe').forEach(function(f){
      try {
        var d = f.contentDocument;
        if (!d) return;
        d.querySelectorAll('a[href]').forEach(function(a){
          var href = a.getAttribute('href') || '';
          var m = href.match(/(?:^|\/)(video(?:\.[a-z0-9]+|\d+))(?:\/([^\/?#]*))?/i);
          if (m && !out[m[1]]) {
            var t = (a.getAttribute('title') || a.textContent || '').trim().slice(0, 140);
            out[m[1]] = [m[1], m[2] || '', t];
          }
        });
      } catch(e) {}
    });
  } catch(e) {}
  var items = Object.values(out);
  // 数据经 document.cookie 回传：先整体编码，再切 1000 字符分片（编码后不会超 4KB 限制）
  var enc = encodeURIComponent(JSON.stringify(items));
  var parts = Math.ceil(enc.length / 1000);
  for (var i = 0; i < parts && i < 60; i++) {
    document.cookie = 'xvc' + i + '=' + enc.substr(i * 1000, 1000) + ';path=/';
  }
  document.cookie = 'xvcn=' + parts + ';path=/';
  for (var j = parts; j < parts + 3; j++) {
    document.cookie = 'xvc' + j + '=; Max-Age=0; path=/';
  }
  // JS 翻页：仅当网格已渲染出视频链接时才点下一页，避免打断加载
  try {
    var vids = document.querySelectorAll('#page-videos-content a[href*="video."]');
    var pag = document.querySelectorAll('#page-videos-content .pagination li a');
    var cur = window.__xvp || 0;
    if (vids.length > 0 && pag.length > 1 && cur < pag.length - 1) {
      window.__xvp = cur + 1;
      var target = pag[cur + 1];
      if (target && (target.getAttribute('href') || '').charAt(0) === '#') { target.click(); }
    }
  } catch(e) {}
  window.scrollTo(0, document.body.scrollHeight);
})();
"#;

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

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// 渲染式抓取：复用登录窗口的内容视图（该视图验证可用），
/// 导航到收藏页后注入提取脚本滚动收集。每有新增就通过 on_page 流出。
pub fn scrape_list(
    app: &AppHandle,
    site: &str,
    cfg: &crate::config::Config,
    list_url: &str,
    on_page: &(dyn Fn(Vec<VideoEntry>) + Send + Sync),
) -> Result<Vec<VideoEntry>> {
    let win_label = site.to_string();
    let content_label = match crate::auth::content_label(site) {
        Some(l) => l,
        None => return Err(anyhow!("登录窗口未打开，请先点登录")),
    };

    // 登录窗口必须已打开（内容视图由 open_login 创建）
    if app.get_webview(&content_label).is_none() {
        return Err(anyhow!("登录窗口未打开，请先点登录"));
    }
    // 等待 Cookie 回填完成，避免抓取与回填竞态
    if !crate::auth::wait_ready(site, std::time::Duration::from_secs(30)) {
        crate::auth::log_debug("scrape: ready timeout (continue anyway)");
    }

    // 被动收集模式：不代替用户导航（程序导航时站点 SPA 会卡在加载态），
    // 用户在登录窗口里自行打开收藏页并翻页，注入的提取脚本自动收集每一页。
    // 每轮通过 on_page 流出新增条目，直到连续多轮无新增或达到轮次上限。
    let js = EXTRACT_JS.to_string();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<VideoEntry> = Vec::new();
    let mut stale_rounds = 0u32;
    let mut rounds = 0u32;

    while rounds < 90 && stale_rounds < 10 {
        rounds += 1;
        if app.get_webview(&content_label).is_none() {
            break; // 窗口被用户关闭
        }
        let mut added = 0usize;
        // eval 注入 + cookie 读回（都必须在主线程）
        let app3 = app.clone();
        let label3 = content_label.clone();
        let js3 = js.clone();
        let result_rx = mpsc::channel::<Vec<RawItem>>();
        let rtx = result_rx.0;
        let _ = app.run_on_main_thread(move || {
            let mut items = Vec::new();
            if let Some(wv) = app3.get_webview(&label3) {
                if wv.eval(&js3).is_ok() {
                    if let Ok(raw) = wv.cookies() {
                        let get = |name: &str| -> Option<String> {
                            raw.iter()
                                .find(|c| c.name() == name)
                                .map(|c| c.value().to_string())
                        };
                        if let Some(nstr) = get("xvcn") {
                            if let Ok(nparts) = nstr.parse::<usize>() {
                                let mut data = String::new();
                                let mut complete = true;
                                for i in 0..nparts {
                                    match get(&format!("xvc{i}")) {
                                        Some(v) => data.push_str(&v),
                                        None => {
                                            complete = false;
                                            break;
                                        }
                                    }
                                }
                                if complete {
                                    let decoded = urldecode(&data);
                                    if let Ok(list) =
                                        serde_json::from_str::<Vec<[String; 3]>>(&decoded)
                                    {
                                        items = list
                                            .into_iter()
                                            .map(|a| RawItem {
                                                id: a[0].clone(),
                                                u: a[1].clone(),
                                                t: a[2].clone(),
                                            })
                                            .collect();
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let _ = rtx.send(items);
        });
        if let Ok(items) = result_rx.1.recv_timeout(Duration::from_millis(2000)) {
            for it in items {
                if seen.insert(it.id.clone()) {
                    out.push(VideoEntry {
                        site: site.to_string(),
                        id: it.id.clone(),
                        url: if it.u.is_empty() {
                            format!("https://www.xvideos.com/{}", it.id)
                        } else {
                            format!("https://www.xvideos.com/{}/{}", it.id, it.u)
                        },
                        title: it.t,
                        thumb: None,
                    });
                    added += 1;
                }
            }
        }
        if added == 0 {
            stale_rounds += 1;
        } else {
            stale_rounds = 0;
            on_page(out[out.len() - added..].to_vec());
        }
    }

    if out.is_empty() {
        return Err(anyhow!(
            "未从页面提取到任何视频——请确认已登录且收藏页地址正确"
        ));
    }
    Ok(out)
}

// ═══════════════ 通用站点的渲染式抓取（规格驱动） ═══════════════
//
// 纯 HTTP 抓取在很多站点走不通（Cloudflare 拦截、列表页 JS 渲染），
// 这条路复用登录窗口的内容视图：真浏览器 + 登录态 + 代理，逐页导航、
// 注入收集脚本、数据经 document.cookie 回传（与 xvideos 被动抓取同通道，
// 前缀 srcc 区分）。选择器和 id 正则来自站点规格，逻辑零站点耦合。

/// 构造收集脚本：把站点规格的选择器/正则以 JSON 字面量安全嵌入。
/// 规格声明了 list_container 时只在容器内收集（页头菜单等区域的
/// 视频链接一律忽略），容器还没渲染出来就返回空，由外层轮询等待。
fn collector_js(spec: &crate::site::generic::SiteSpec) -> String {
    let idre = serde_json::to_string(spec.id_re).unwrap_or_else(|_| "''".into());
    let scope = serde_json::to_string(spec.list_container).unwrap_or_else(|_| "''".into());
    format!(
        r#"(function(){{
  try {{
    document.querySelectorAll('button, .btn, [role=button], input[type=button], input[type=submit]').forEach(function(b){{
      if (!b.offsetParent && !b.offsetWidth) return;
      var tx = (b.textContent || b.value || '').toLowerCase();
      if (/accept|agree|enter|i.?m 18|i am 18|over 18|continue|同意|进入|我已满/.test(tx)) {{ b.click(); }}
    }});
  }} catch(e) {{}}
  try {{
    var re = new RegExp({idre});
    var scope = {scope};
    var roots = scope ? document.querySelectorAll(scope) : [document];
    var out = {{}};
    function grab(li) {{
      li.querySelectorAll('a[href]').forEach(function(a){{
        var href = a.href || '';
        var m = href.match(re);
        if (!m) return;
        var img = a.querySelector('img');
        var t = (a.getAttribute('title') || (img ? (img.getAttribute('alt') || '') : '') || '').trim().slice(0, 140);
        if (!out[m[1]]) out[m[1]] = [m[1], href, t, img ? (img.currentSrc || img.src || '') : ''];
      }});
    }}
    for (var r = 0; r < roots.length; r++) {{
      var lis = roots[r].querySelectorAll('li');
      if (lis.length === 0) {{ grab(roots[r]); continue; }}
      for (var i = 0; i < lis.length; i++) {{
        var cls = (lis[i].className || '') + '';
        // 列表末尾站点会追加"推荐/相关"区块（同一容器内）：遇到即停止,
        // 避免把推荐视频当成收藏条目
        if (/recommend|divider|related|suggested|banner|advert/i.test(cls)) break;
        grab(lis[i]);
      }}
    }}
    var items = Object.values(out);
    var enc = encodeURIComponent(JSON.stringify(items));
    var parts = Math.ceil(enc.length / 1000);
    for (var i = 0; i < parts && i < 80; i++) {{
      document.cookie = 'srcc' + i + '=' + enc.substr(i * 1000, 1000) + ';path=/';
    }}
    document.cookie = 'srccn=' + parts + ';path=/';
    for (var j = parts; j < parts + 3; j++) {{
      document.cookie = 'srcc' + j + '=; Max-Age=0; path=/';
    }}
  }} catch(e) {{}}
}})();"#
    )
}

/// 从内容视图的 Cookie 里读回收集结果（srccn = 分片数，srcc{i} = 分片）
fn read_collected(wv: &tauri::Webview) -> Option<Vec<[String; 4]>> {
    let raw = wv.cookies().ok()?;
    let get = |name: &str| -> Option<String> {
        raw.iter()
            .find(|c| c.name() == name)
            .map(|c| c.value().to_string())
    };
    let n: usize = get("srccn")?.parse().ok()?;
    let mut data = String::new();
    for i in 0..n {
        data.push_str(&get(&format!("srcc{i}"))?);
    }
    serde_json::from_str(&urldecode(&data)).ok()
}

pub fn scrape_rendered(
    app: &AppHandle,
    spec: &crate::site::generic::SiteSpec,
    list_url: &str,
    on_page: &(dyn Fn(Vec<VideoEntry>) + Send + Sync),
) -> Result<Vec<VideoEntry>> {
    let content_label = crate::auth::content_label(spec.id)
        .ok_or_else(|| anyhow!("该站点未打开登录窗口——请先点「登录」登录后重试"))?;
    let wv = app
        .get_webview(&content_label)
        .ok_or_else(|| anyhow!("登录窗口已关闭——请重新打开登录窗口再抓取"))?;
    let _ = crate::auth::wait_ready(spec.id, Duration::from_secs(10));

    let base = list_url.trim_end_matches('/').to_string();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<VideoEntry> = Vec::new();
    let js = collector_js(spec);

    /// 一轮"注入收集器 → 读回条目 → 去重入库"的共用逻辑，返回本轮新增条目
    macro_rules! collect_round {
        ($wv:expr, $js:expr, $seen:expr, $out:expr, $spec:expr) => {{
            let _ = $wv.eval("document.cookie = 'srccn=; Max-Age=0; path=/';");
            let _ = $wv.eval(&$js);
            let mut items: Vec<[String; 4]> = Vec::new();
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(400));
                if let Some(list) = read_collected(&$wv) {
                    items = list;
                    break;
                }
            }
            let mut batch: Vec<VideoEntry> = Vec::new();
            for it in items {
                if $seen.insert(it[0].clone()) {
                    // 站点要求归一化（如子域 → 主域）时替换为规范 URL
                    let url = match $spec.canon_url {
                        Some(canon) => canon(&$spec.origin, &it[0]),
                        None => it[1].clone(),
                    };
                    $out.push(VideoEntry {
                        site: $spec.id.into(),
                        id: it[0].clone(),
                        url,
                        title: it[2].clone(),
                        thumb: if it[3].is_empty() { None } else { Some(it[3].clone()) },
                    });
                    batch.push($out[$out.len() - 1].clone());
                }
            }
            batch
        }};
    }

    if spec.page_style == crate::site::generic::PageStyle::Scroll {
        // 滚动加载模式：URL 翻页不存在（?page=N 直接 404），列表由站点 JS
        // 无限滚动追加。停在列表页反复"滚动到底 → 注入收集器"，连续多轮
        // 无新增即认为到头。容器未渲染出来时收集为空，靠 stale 计数兜底等待。
        let target: tauri::Url = base.clone().parse()?;
        wv.navigate(target.clone())?;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(300));
            if let Ok(u) = wv.url() {
                if u.as_str() == target.as_str() {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2000));
        let mut stale = 0u32;
        let mut rounds = 0u32;
        while rounds < 120 && stale < 10 {
            rounds += 1;
            let batch = collect_round!(wv, js, seen, out, spec);
            let cur_url = wv.url().map(|u| u.as_str().to_string()).unwrap_or_default();
            crate::auth::log_debug(&format!(
                "scroll round {rounds}: +{} total={} stale={stale} url={cur_url}",
                batch.len(),
                out.len()
            ));
            if batch.is_empty() {
                stale += 1;
            } else {
                stale = 0;
                on_page(batch);
            }
            // 滚动触发无限加载（滚两步：中段预热 + 到底）
            let _ = wv.eval("window.scrollTo(0, Math.max(600, document.body.scrollHeight / 2))");
            std::thread::sleep(Duration::from_millis(700));
            let _ = wv.eval("window.scrollTo(0, document.body.scrollHeight)");
            std::thread::sleep(Duration::from_millis(1600));
        }
    } else {
        for page in 1..300u32 {
            let url = if page == 1 {
                base.clone()
            } else {
                crate::site::generic::page_url(&base, spec.page_style, page)
            };
            let target: tauri::Url = url.parse()?;
            wv.navigate(target.clone())?;
            // 等导航落地（最多 ~12s），再留渲染时间
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(300));
                if let Ok(u) = wv.url() {
                    if u.as_str() == target.as_str() {
                        break;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(1800));
            // 滚动两轮触发懒加载（缩略图/后续条目）
            let _ = wv.eval("window.scrollTo(0, Math.max(600, document.body.scrollHeight / 2))");
            std::thread::sleep(Duration::from_millis(900));
            let _ = wv.eval("window.scrollTo(0, document.body.scrollHeight)");
            std::thread::sleep(Duration::from_millis(900));
            let batch = collect_round!(wv, js, seen, out, spec);
            if batch.is_empty() {
                break; // 翻页结束（或页面没渲染出列表）
            }
            on_page(batch);
            std::thread::sleep(Duration::from_millis(1200));
        }
    }

    if out.is_empty() {
        return Err(anyhow!(
            "未从页面提取到任何视频——请确认已登录、收藏页地址正确，且登录窗口保持打开"
        ));
    }
    Ok(out)
}

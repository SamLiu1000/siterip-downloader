//! 规格驱动的通用站点适配器：下载全部交给 yt-dlp（各站点都有官方提取器），
//! 收藏/列表抓取用每站一份的"链接正则 + 分页风格"做纯 HTTP 收集。
//!
//! 登录复用现有的登录窗口（cookie_domain 决定 Cookie 归属），HTTP 请求
//! 经 http::get_with_retry 自动附带已保存的 Cookie，登录后收藏页即可抓。
//!
//! 链接正则统一约定两个捕获组：(1) 相对路径或完整 URL，(2) 视频 id。
//! 没做站点的浏览式被动抓取（那套依赖 xvideos 的专用 cookie 协议）——
//! HTTP 抓不到时直接报错提示先登录或检查收藏页地址。

use super::{SiteAdapter, VideoEntry};
use crate::config::Config;
use anyhow::Result;
use async_trait::async_trait;
use tauri::AppHandle;

/// 分页风格：路径式 base/{n}（thisvid）或查询式 base?page={n}（xhamster 等）；
/// Scroll = 不做 URL 翻页，列表由站点 JS 无限滚动加载（渲染式抓取时反复滚动收集）
#[derive(Clone, Copy, PartialEq)]
pub enum PageStyle {
    Path,
    Query,
    Scroll,
}

pub struct SiteSpec {
    pub id: &'static str,
    pub display: &'static str,
    pub home: &'static str,
    pub cookie_domain: &'static str,
    /// 视频链接正则：(1) 路径/URL (2) 视频 id（HTTP 抓取用）
    pub link_re: &'static str,
    /// 站点首页 origin（相对路径补全用）
    pub origin: &'static str,
    /// 收藏/列表页的分页风格
    pub page_style: PageStyle,
    /// 渲染式抓取：视频锚点的 CSS 选择器（逗号可多选）
    pub item_selector: &'static str,
    /// 渲染式抓取：从绝对 href 里提取视频 id 的 JS 正则（捕获组 1 = id）
    pub id_re: &'static str,
    /// 可选：把条目 URL 归一化成规范形式（如 cn 子域 → www 主域，viewkey 全站通用）
    pub canon_url: Option<fn(&str, &str) -> String>,
    /// 真实列表容器的 CSS 选择器（逗号分隔）。非空时 HTTP 抓取只扫描容器之后
    /// 的页面片段、渲染式收集只取容器内的链接——排除页头"热门"菜单等垃圾锚点
    pub list_container: &'static str,
    /// 用户未配置收藏地址时的默认收藏页（空 = 无默认，须用户填写）
    pub default_fav: &'static str,
}

// ---- 各站点规格（下载由 yt-dlp 的同名提取器负责）----
// 分页抓不到时用户可以直接把更深的列表页地址粘进"收藏页地址"再来一次
pub static PORNHUB: SiteSpec = SiteSpec {
    id: "pornhub",
    display: "PornHub",
    home: "https://www.pornhub.com/",
    cookie_domain: "pornhub.com",
    // view_video.php?viewkey=phXXXX（老式）或纯十六进制（新式，无 ph 前缀）；
    // href 可能是相对路径（cn./www. 子域页面均为如此）
    link_re: r#"href="((?:https?://[^"]*?)?/?view_video\.php\?viewkey=((?:ph)?[a-f0-9]{8,}))""#,
    origin: "https://www.pornhub.com",
    // 列表由 JS 无限滚动加载（?page=N 会 404），且页头"热门"菜单里也有
    // viewkey 链接——必须只认列表容器内的锚点
    page_style: PageStyle::Scroll,
    item_selector: "a[href*='view_video.php'], a[href*='viewkey=']",
    id_re: r#"viewkey=((?:ph)?[a-f0-9]{8,})"#,
    // cn 等子域的 viewkey 在 www 主域同样有效，归一化后 yt-dlp 提取更稳
    canon_url: Some(|_origin: &str, id: &str| {
        format!("https://www.pornhub.com/view_video.php?viewkey={id}")
    }),
    list_container: "#moreData, .likedVideosWrapper, #videoCategory",
    default_fav: "https://cn.pornhub.com/likedvideos",
};
pub static XHAMSTER: SiteSpec = SiteSpec {
    id: "xhamster",
    display: "xHamster",
    home: "https://xhamster.com/",
    cookie_domain: "xhamster.com",
    // /videos/<slug>-<数字id>
    link_re: r#"href="((?:https?://(?:[^"/]+\.)?)?xhamster(?:\d*)\.com/videos/[^"]*?)(\d{5,})(?:["/?])"#,
    origin: "https://xhamster.com",
    page_style: PageStyle::Query,
    item_selector: "a[href*='/videos/']",
    id_re: r#"(?:xhamster\d*\.com)?/videos/[^?]*?-([0-9]{5,})"#,
    canon_url: None,
    list_container: "",
    default_fav: "",
};
pub static XNXX: SiteSpec = SiteSpec {
    id: "xnxx",
    display: "XNXX",
    home: "https://www.xnxx.com/",
    cookie_domain: "xnxx.com",
    // /video-<token>/<slug>
    link_re: r#"href="(https?://(?:www\.)?xnxx\.com/(video-[a-z0-9]+)[^"]*)""#,
    origin: "https://www.xnxx.com",
    page_style: PageStyle::Path,
    item_selector: "a[href*='xnxx.com/video-'], a[href^='/video-']",
    id_re: r#"xnxx\.com/video-([a-z0-9]{4,})"#,
    canon_url: None,
    list_container: "",
    default_fav: "",
};
pub static THISVID: SiteSpec = SiteSpec {
    id: "thisvid",
    display: "ThisVid",
    home: "https://thisvid.com/",
    cookie_domain: "thisvid.com",
    // /videos/<数字id>/<slug>/
    link_re: r#"href="(https?://thisvid\.com/videos/(\d+)/[^"]*)""#,
    origin: "https://thisvid.com",
    page_style: PageStyle::Path,
    item_selector: "a[href*='/videos/']",
    id_re: r#"thisvid\.com/videos/(\d+)"#,
    canon_url: None,
    list_container: "",
    default_fav: "",
};
pub static SPANKBANG: SiteSpec = SiteSpec {
    id: "spankbang",
    display: "SpankBang",
    home: "https://spankbang.com/",
    cookie_domain: "spankbang.com",
    // /<id>/video/<slug>
    link_re: r#"href="(https?://(?:www\.)?spankbang\.com/([a-z0-9]{5,})/video/[^"]*)""#,
    origin: "https://spankbang.com",
    page_style: PageStyle::Query,
    item_selector: "a[href*='/video/']",
    id_re: r#"spankbang\.com/([a-z0-9]+)/video/"#,
    canon_url: None,
    list_container: "",
    default_fav: "",
};
pub static EPORNER: SiteSpec = SiteSpec {
    id: "eporner",
    display: "EPorner",
    home: "https://www.eporner.com/",
    cookie_domain: "eporner.com",
    // /hd-porn/<id>/<slug>
    link_re: r#"href="(https?://(?:www\.)?eporner\.com/(?:hd-porn|video-)/([a-z0-9]{6,})[^"]*)""#,
    origin: "https://www.eporner.com",
    page_style: PageStyle::Query,
    item_selector: "a[href*='/hd-porn/'], a[href*='/video-']",
    id_re: r#"eporner\.com/(?:hd-porn|video-)/([a-z0-9]{4,})"#,
    canon_url: None,
    list_container: "",
    default_fav: "",
};
pub static YOUPORN: SiteSpec = SiteSpec {
    id: "youporn",
    display: "YouPorn",
    home: "https://www.youporn.com/",
    cookie_domain: "youporn.com",
    // /watch/<数字id>/<slug>
    link_re: r#"href="(https?://(?:www\.)?youporn\.com/watch/(\d+)[^"]*)""#,
    origin: "https://www.youporn.com",
    page_style: PageStyle::Query,
    item_selector: "a[href*='/watch/']",
    id_re: r#"youporn\.com/watch/(\d+)"#,
    canon_url: None,
    list_container: "",
    default_fav: "",
};
pub static REDTUBE: SiteSpec = SiteSpec {
    id: "redtube",
    display: "RedTube",
    home: "https://www.redtube.com/",
    cookie_domain: "redtube.com",
    // /<数字id>/<slug>
    link_re: r#"href="(https?://(?:www\.)?redtube\.com/(\d{6,})[^"]*)""#,
    origin: "https://www.redtube.com",
    page_style: PageStyle::Query,
    item_selector: "a[href*='redtube']",
    id_re: r#"redtube\.com/(\d{6,})"#,
    canon_url: None,
    list_container: "",
    default_fav: "",
};

macro_rules! spec_of {
    ($name:ident, $spec:ident) => {
        pub struct $name;
        #[async_trait]
        impl SiteAdapter for $name {
            fn id(&self) -> &'static str {
                $spec.id
            }
            fn display(&self) -> &'static str {
                $spec.display
            }
            fn home_url(&self) -> &'static str {
                $spec.home
            }
            fn cookie_domain(&self) -> &'static str {
                $spec.cookie_domain
            }
            fn default_favorites_url(&self) -> &'static str {
                $spec.default_fav
            }
            async fn list_favorites(
                &self,
                app: &AppHandle,
                cfg: &Config,
                list_url: &str,
                on_page: &(dyn Fn(Vec<VideoEntry>) + Send + Sync),
            ) -> Result<usize> {
                // 主路径：纯 HTTP 分页抓取；抓不到（Cloudflare/JS 渲染/被拦）
                // 就回退到登录窗口的渲染式抓取，两条路共用同一份站点规格。
                // Scroll 型站点（列表 JS 无限加载）：HTTP 只能拿到服务器渲染的
                // 首屏真实条目——先流出这些，然后仍走渲染式滚动抓完整列表，
                // 结果按 id 去重合并（前端与数据库都按 id 去重）。
                let http_res = generic_list(&$spec, cfg, list_url, on_page).await;
                let http_total = match &http_res {
                    Ok(n) => *n,
                    Err(_) => 0,
                };
                match http_res {
                    Ok(n) if n > 0 && $spec.page_style != PageStyle::Scroll => Ok(n),
                    other => {
                        crate::auth::log_debug(&format!(
                            "{} http path returned {other:?}, falling back to rendered scrape",
                            $spec.id
                        ));
                        match crate::scrape::scrape_rendered(app, &$spec, list_url, on_page) {
                            Ok(list) if !list.is_empty() => Ok(list.len()),
                            _ if http_total > 0 => Ok(http_total),
                            _ => Err(anyhow::anyhow!(
                                "未从页面提取到任何视频——请先点「登录」确认已登录，再检查收藏页地址"
                            )),
                        }
                    }
                }
            }
        }
    };
}

spec_of!(PornHub, PORNHUB);
spec_of!(XHamster, XHAMSTER);
spec_of!(Xnxx, XNXX);
spec_of!(ThisVid, THISVID);
spec_of!(SpankBang, SPANKBANG);
spec_of!(Eporner, EPORNER);
spec_of!(YouPorn, YOUPORN);
spec_of!(RedTube, REDTUBE);

pub(crate) fn page_url(base: &str, style: PageStyle, page: u32) -> String {
    let base = base.trim_end_matches('/');
    match style {
        PageStyle::Query => format!("{base}?page={page}"),
        PageStyle::Path => {
            if base.contains('?') {
                format!("{base}&page={page}")
            } else {
                format!("{base}/{page}")
            }
        }
        // 滚动加载：没有 URL 翻页（HTTP 路径只取第一页，完整列表走渲染式滚动）
        PageStyle::Scroll => base.to_string(),
    }
}

/// 站点声明了 list_container 时，只扫描首个容器标记之后的内容：
/// 很多站点的页头导航（"热门"下拉菜单等）里也塞满视频链接，
/// 不切片会把菜单垃圾当成收藏条目。容器标记找不到 = 列表是 JS 渲染的，
/// 返回空片段让上层走渲染式抓取。
fn scope_body<'a>(spec: &SiteSpec, body: &'a str) -> &'a str {
    if spec.list_container.is_empty() {
        return body;
    }
    let mut best: Option<usize> = None;
    for tok in spec.list_container.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let needle = tok.trim_start_matches(['#', '.']);
        if let Some(p) = body.find(needle) {
            best = Some(best.map_or(p, |b| b.min(p)));
        }
    }
    match best {
        Some(p) => &body[p..],
        None => "",
    }
}

async fn generic_list(
    spec: &SiteSpec,
    cfg: &Config,
    list_url: &str,
    on_page: &(dyn Fn(Vec<VideoEntry>) + Send + Sync),
) -> Result<usize> {
    use rand::Rng;
    let client = crate::http::client_for(cfg, spec.id)?;
    let link_re = regex::Regex::new(spec.link_re)?;
    let title_re = regex::Regex::new(r#"title="([^"]{2,140})""#)?;
    let thumb_re = regex::Regex::new(
        r#"(?:data-src|src)="(https?://[^"]+?\.(?:jpg|webp|png)[^"]*)""#,
    )?;

    let base = list_url.trim_end_matches('/').to_string();
    let mut seen = std::collections::HashSet::new();
    let mut total = 0usize;

    for page in 1..400u32 {
        let url = if page == 1 {
            base.clone()
        } else {
            page_url(&base, spec.page_style, page)
        };
        let body = crate::http::get_with_retry(&client, &url).await?;
        let full_body = body;
        let body = scope_body(spec, &full_body);
        if body.is_empty() {
            break; // 容器标记不存在：列表由 JS 渲染，HTTP 拿不到真实条目
        }
        let mut page_items: Vec<VideoEntry> = Vec::new();
        let hits: Vec<(std::ops::Range<usize>, String, String)> = link_re
            .captures_iter(&body)
            .filter_map(|c| {
                let mut path = c[1].to_string();
                if path.starts_with('/') {
                    path = format!("{}{}", spec.origin, path);
                }
                // 站点要求归一化（如子域 → 主域）时替换为规范 URL
                if let Some(canon) = spec.canon_url {
                    path = canon(&spec.origin, &c[2]);
                }
                Some((c.get(0).unwrap().range(), path, c[2].to_string()))
            })
            .collect();
        for (i, (range, url, id)) in hits.iter().enumerate() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let win_end = hits
                .get(i + 1)
                .map(|(nr, _, _)| nr.start)
                .unwrap_or((range.end + 3000).min(body.len()));
            let title = title_re
                .captures(&body[range.end..win_end])
                .map(|t| t[1].to_string())
                .unwrap_or_default();
            let thumb = thumb_re
                .captures(&body[range.end..win_end])
                .map(|t| t[1].to_string())
                .map(|u| u.replace("&amp;", "&"));
            page_items.push(VideoEntry {
                site: spec.id.into(),
                id: id.clone(),
                url: url.clone(),
                title: title
                    .replace("&amp;", "&")
                    .replace("&quot;", "\"")
                    .replace("&#39;", "'"),
                thumb,
            });
        }
        let added = page_items.len();
        total += added;
        if added > 0 {
            on_page(page_items);
        }
        if added == 0 {
            break; // 翻页结束（或第一页就没抓到：多半是地址不对/未登录）
        }
        let delay = rand::thread_rng().gen_range(800..1600);
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    Ok(total)
}

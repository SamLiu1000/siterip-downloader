use super::{SiteAdapter, VideoEntry};
use crate::config::Config;
use anyhow::Result;
use async_trait::async_trait;
use tauri::AppHandle;

pub struct Xvideos;

const DOMAIN: &str = "xvideos.com";

#[async_trait]
impl SiteAdapter for Xvideos {
    fn id(&self) -> &'static str {
        "xvideos"
    }
    fn display(&self) -> &'static str {
        "XVIDEOS"
    }
    fn home_url(&self) -> &'static str {
        "https://www.xvideos.com/"
    }
    fn cookie_domain(&self) -> &'static str {
        DOMAIN
    }
    fn default_favorites_url(&self) -> &'static str {
        "https://www.xvideos.com/videos-i-like"
    }

    /// 主路径：纯 HTTP 分页抓取（/videos-i-like/<n> 为服务器端渲染，每页约 28 条）；
    /// HTTP 结果为空时回退到被动收集（用户在登录窗口里浏览）
    async fn list_favorites(
        &self,
        app: &AppHandle,
        cfg: &Config,
        list_url: &str,
        on_page: &(dyn Fn(Vec<VideoEntry>) + Send + Sync),
    ) -> Result<usize> {
        crate::auth::log_debug("adapter list_favorites: enter");
        match self.list_favorites_http(cfg, list_url, on_page).await {
            Ok(n) if n > 0 => Ok(n),
            other => {
                crate::auth::log_debug(&format!(
                    "http path returned {other:?}, falling back to passive scrape"
                ));
                let list = crate::scrape::scrape_list(app, self.id(), cfg, list_url, on_page)?;
                Ok(list.len())
            }
        }
    }
}

impl Xvideos {
    async fn list_favorites_http(
        &self,
        cfg: &Config,
        list_url: &str,
        on_page: &(dyn Fn(Vec<VideoEntry>) + Send + Sync),
    ) -> Result<usize> {
        use rand::Rng;
        let client = crate::http::client_for(cfg, self.id())?;
        let href_re = regex::Regex::new(
            r#"(?:href="|href=')(?:https://www\.xvideos\.com)?/(video\.[a-z0-9]+|\d{6,})/([^"?#]*)"#,
        )?;
        let title_re = regex::Regex::new(r#"title="([^"]{2,140})""#)?;
        // 缩略图：xvideos 列表页图片在 *.xvideos-cdn.com，懒加载时真实地址在
        // data-src（src 是 lightbox-blank.gif 占位图），jpg/webp/png
        let thumb_re = regex::Regex::new(
            r#"(?:data-src|src)="(https://[^"]*xvideos-cdn\.com/[^"]+?\.(?:jpg|webp|png))""#,
        )?;

        let base = list_url.trim_end_matches('/').to_string();
        let mut seen = std::collections::HashSet::new();
        let mut total = 0usize;

        // 分页从 /1 开始（base 页是 JS 空壳，不含列表）
        crate::auth::log_debug("http path: starting pagination");
        for page in 1..400u32 {
            let url = format!("{}/{}", base, page);
            let body = crate::http::get_with_retry(&client, &url).await?;
            if page == 1 {
                crate::auth::log_debug(&format!(
                    "http page1: {} bytes, tokens={}",
                    body.len(),
                    body.matches("video.").count()
                ));
            }
            if body.contains("登录") && body.contains("还没有视频") && page == 0 {
                return Err(anyhow::anyhow!("站点返回未登录状态——请先在程序里完成登录"));
            }
            let mut page_items: Vec<VideoEntry> = Vec::new();
            // 先收集所有视频链接的位置，缩略图取"本链接到下个链接之间"
            // 窗口内的第一张 CDN 图片（xvideos 的 <a> 包着 <img class=thumb>）
            let href_matches: Vec<(std::ops::Range<usize>, String, String)> = href_re
                .captures_iter(&body)
                .filter_map(|c| {
                    let token = c[1].to_string();
                    if token == "videos" {
                        return None;
                    }
                    Some((c.get(0).unwrap().range(), token, c[2].to_string()))
                })
                .collect();
            let mut spans: Vec<(&std::ops::Range<usize>, &String, &String)> =
                href_matches.iter().map(|(r, t, s)| (r, t, s)).collect();
            spans.sort_by_key(|(r, _, _)| r.start);
            for (i, (range, token, slug)) in spans.iter().enumerate() {
                if !seen.insert((*token).clone()) {
                    continue;
                }
                let win_end = spans
                    .get(i + 1)
                    .map(|(nr, _, _)| nr.start)
                    .unwrap_or(body.len().min(range.end + 3000));
                // 标题：href 之后 300 字符内的第一个 title 属性；否则用 slug
                // 标题优先用 URL slug（title 属性常是画质标签）
                let title = if slug.is_empty() {
                    title_re
                        .captures(&body[range.end..])
                        .map(|t| t[1].to_string())
                        .unwrap_or_default()
                } else {
                    slug.replace('_', " ")
                };
                let thumb = thumb_re
                    .captures(&body[range.end..win_end])
                    .map(|t| t[1].to_string())
                    .filter(|u| !u.contains("blank") && !u.contains("lightbox"))
                    .map(|u| html_unescape(&u));
                page_items.push(VideoEntry {
                    site: self.id().into(),
                    id: (*token).clone(),
                    url: format!("https://www.xvideos.com/{}/{}", token, slug),
                    title: html_unescape(&title),
                    thumb,
                });
            }
            let added = page_items.len();
            total += added;
            if added > 0 {
                on_page(page_items);
            }
            if added == 0 {
                break; // 翻页结束
            }
            let delay = rand::thread_rng().gen_range(800..1600);
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        Ok(total)
    }
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

pub fn validate_favorites_url(url: &str) -> Result<()> {
    if url.contains("/favorite/") || url.contains("videos-i-like") || url.contains("my-videos") {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "收藏页地址应形如 https://www.xvideos.com/videos-i-like 或 /favorite/<id>/<名称>"
        ))
    }
}

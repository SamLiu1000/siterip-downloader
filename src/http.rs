use crate::config::Config;
use anyhow::{anyhow, Result};
use std::time::Duration;

/// 为指定站点构建带代理与登录 Cookie 的 reqwest 客户端
pub fn client_for(cfg: &Config, site: &str) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .timeout(Duration::from_secs(30));
    if let Some(proxy) = cfg.proxy_for(site) {
        b = b.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(b.build()?)
}

/// GET，带 Cookie 头；失败自动重试（借鉴 SCrawler Wait429：指数退避）
pub async fn get_with_retry(client: &reqwest::Client, url: &str) -> Result<String> {
    let cookies = crate::cookies::load()?;
    let mut delay = 1000u64;
    for attempt in 0..5 {
        let host = url
            .split('/')
            .nth(2)
            .ok_or_else(|| anyhow!("非法 URL: {url}"))?
            .to_string();
        let mut req = client.get(url);
        let hv = crate::cookies::header_value(&cookies, &host);
        if !hv.is_empty() {
            req = req.header(reqwest::header::COOKIE, hv);
        }
        match req.send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(r) => return Ok(r.text().await?),
                Err(e) if e.status().is_some_and(|s| s.as_u16() == 429) => {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    delay = delay * 2 + 1000;
                }
                Err(e) => return Err(e.into()),
            },
            Err(e) => {
                if attempt == 4 {
                    return Err(e.into());
                }
                tokio::time::sleep(Duration::from_millis(delay)).await;
                delay = delay * 2 + 1000;
            }
        }
    }
    Err(anyhow!("重试次数用尽: {url}"))
}

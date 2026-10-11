use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// unix 秒，0 = 会话 cookie
    #[serde(default)]
    pub expiry: i64,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub same_site: String,
}

pub fn cookie_file() -> PathBuf {
    crate::config::data_dir().join("cookies.json")
}

pub fn cookies_txt_path() -> PathBuf {
    cookie_file().with_extension("txt")
}

pub fn debug_log_path() -> PathBuf {
    cookie_file().with_extension("log")
}

pub fn load() -> Result<Vec<Cookie>> {
    let p = cookie_file();
    if !p.exists() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&fs::read_to_string(&p)?)?)
}

pub fn save(cookies: &[Cookie]) -> Result<()> {
    let p = cookie_file();
    fs::create_dir_all(p.parent().unwrap())?;
    // 合并保存：webview 当前读取可能处于未登录态（缺失登录 Cookie），
    // 不能让轮询覆盖掉已保存的登录凭据
    let mut merged: Vec<Cookie> = cookies.to_vec();
    if let Ok(old) = serde_json::from_str::<Vec<Cookie>>(&fs::read_to_string(&p)?) {
        for o in old {
            if o.name.starts_with("xv") {
                continue; // 诊断 cookie 不保留
            }
            if !merged.iter().any(|c| c.name == o.name) {
                merged.push(o);
            }
        }
    }
    // 去重：同名（+同域同路径）cookie 只保留一条。webview 一次读取可能
    // 返回多条同名不同值的 cookie（如轮换的 session token），重复注入时
    // 生效的是任意一条，可能恰是未登录的旧值。
    let mut seen = std::collections::HashSet::new();
    merged.retain(|c| seen.insert((c.name.clone(), c.domain.clone(), c.path.clone())));
    fs::write(&p, serde_json::to_string_pretty(&merged)?)?;
    write_netscape(&merged)?;
    Ok(())
}

pub fn has_login() -> bool {
    cookie_file().exists()
}

/// 生成 Netscape cookies.txt，供 yt-dlp 使用。
/// 第二列 TRUE 表示域匹配含子域，此时域名必须以 '.' 开头，否则 cookiejar 解析失败。
fn write_netscape(cookies: &[Cookie]) -> Result<()> {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for c in cookies {
        let expiry = if c.expiry > 0 { c.expiry } else { 2147483647 };
        let domain = if c.domain.starts_with('.') {
            c.domain.clone()
        } else {
            format!(".{}", c.domain)
        };
        out.push_str(&format!(
            "{}\tTRUE\t{}\t{}\t{}\t{}\t{}\n",
            domain, c.path, if c.secure { "TRUE" } else { "FALSE" }, expiry, c.name, c.value
        ));
    }
    fs::write(cookies_txt_path(), out)?;
    Ok(())
}

/// 拼成 HTTP Cookie 头，供 reqwest 手动注入
pub fn header_value(cookies: &[Cookie], domain: &str) -> String {
    cookies
        .iter()
        .filter(|c| domain.ends_with(c.domain.trim_start_matches('.')))
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

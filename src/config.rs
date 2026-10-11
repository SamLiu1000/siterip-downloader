use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 全局代理地址（本程序流量均面向目标站点，等价于 per-site 开关）。留空则直连。
    #[serde(default = "default_proxy")]
    pub proxy: String,
    /// 每站点代理覆盖：site id -> proxy url
    #[serde(default)]
    pub per_site_proxy: HashMap<String, String>,
    #[serde(default = "default_download_dir")]
    pub download_dir: PathBuf,
    /// 画质偏好：高度上限（每个视频的实际分辨率由站点提供为准，站点没有的清晰度自动选不超过上限的最优档，0 = 不限）
    #[serde(default = "default_quality")]
    pub quality: u32,
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
    /// 各站点的收藏页地址：site id -> url，形如 https://www.xvideos.com/favorite/<id>/<名称>
    #[serde(default)]
    pub favorites_urls: HashMap<String, String>,
    /// 下载队列全部结束后的动作：none | shutdown（60 秒后关机，可 shutdown /a 取消）| sleep
    #[serde(default)]
    pub after_done: String,
}

fn default_proxy() -> String {
    "http://127.0.0.1:10809".into()
}
fn default_download_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join("siterip")
}
fn default_quality() -> u32 {
    1080
}
fn default_concurrency() -> u32 {
    2
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}

/// 程序数据目录：exe 同级的 data 文件夹（便携式，配置/数据库/Cookie/日志都放这里，
/// 备份或换机 = 拷贝整个程序文件夹）。首次运行时自动把旧位置 %APPDATA%\siterip 的数据迁移过来。
pub fn data_dir() -> PathBuf {
    static MIGRATED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("data");
    let _ = fs::create_dir_all(&dir);
    if MIGRATED.set(()).is_ok() {
        for f in ["config.toml", "siterip.db", "cookies.json", "cookies.txt", "cookies.log"] {
            let dst = dir.join(f);
            if dst.exists() {
                continue; // 新位置已有数据，以它为准
            }
            if let Some(src) = dirs::config_dir().map(|c| c.join("siterip").join(f)) {
                if src.exists() {
                    let _ = fs::copy(&src, &dst);
                }
            }
        }
    }
    dir
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

impl Config {
    pub fn load() -> Result<Self> {
        let p = config_path();
        if p.exists() {
            let s = fs::read_to_string(&p).context("读取配置失败")?;
            Ok(toml::from_str(&s)?)
        } else {
            let cfg = Config::default();
            cfg.save()?;
            Ok(cfg)
        }
    }

    pub fn save(&self) -> Result<()> {
        let p = config_path();
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(&p, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn proxy_for(&self, site: &str) -> Option<&str> {
        if let Some(p) = self.per_site_proxy.get(site) {
            if !p.is_empty() {
                return Some(p);
            }
        }
        if self.proxy.is_empty() {
            None
        } else {
            Some(&self.proxy)
        }
    }
}

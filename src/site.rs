use crate::config::Config;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

pub mod generic;
pub mod xvideos;

/// 一个站点的收藏视频条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoEntry {
    pub site: String,
    pub id: String,
    pub url: String,
    pub title: String,
    /// 缩略图 URL（前端预览用；旧数据/解析失败时为 None）
    #[serde(default)]
    pub thumb: Option<String>,
}

/// 站点适配器：扩展新站点时实现此 trait 并在 `adapter_for` 注册即可
#[async_trait]
pub trait SiteAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn display(&self) -> &'static str;
    fn home_url(&self) -> &'static str;
    /// 需要登录 Cookie 的主域
    fn cookie_domain(&self) -> &'static str;
    /// 用户未配置收藏地址时使用的默认收藏页（空 = 无默认，须用户填写）
    fn default_favorites_url(&self) -> &'static str {
        ""
    }
    /// 站点自带提取器时（yt-dlp 支持），下载直接交给 yt-dlp
    fn ytdlp_supported(&self) -> bool {
        true
    }
    /// 枚举收藏/播放列表（渲染式抓取），每有新增通过 on_page 流出，返回总条数
    async fn list_favorites(
        &self,
        app: &AppHandle,
        cfg: &Config,
        list_url: &str,
        on_page: &(dyn Fn(Vec<VideoEntry>) + Send + Sync),
    ) -> Result<usize>;
}

/// 站点注册表：新站点实现 SiteAdapter 后在这里登记即可（下拉框自动出现）
pub fn adapter_for(site: &str) -> Result<&'static dyn SiteAdapter> {
    all_adapters()
        .into_iter()
        .find(|a| a.id() == site)
        .ok_or_else(|| anyhow!("未注册的站点: {}", site))
}

pub fn all_adapters() -> Vec<&'static dyn SiteAdapter> {
    vec![
        &xvideos::Xvideos,
        &generic::PornHub,
        &generic::XHamster,
        &generic::Xnxx,
        &generic::ThisVid,
        &generic::SpankBang,
        &generic::Eporner,
        &generic::YouPorn,
        &generic::RedTube,
    ]
}

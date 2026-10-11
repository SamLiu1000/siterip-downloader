# siterip — xvideos 收藏批量下载器

Tauri 2 桌面应用（Rust 后端 + WebView2 界面），用 yt-dlp 做下载引擎。
专为"登录自己的账号 → 批量下载标记为喜爱的视频"设计，支持 per-site 代理。

## 首次使用

1. 安装 yt-dlp（已装可跳过）：
   ```
   pip install -U yt-dlp
   ```
2. 启动程序：
   ```
   cargo run --release
   ```
3. 点 **⚙ 设置**，填入收藏页地址（浏览器里打开你的收藏页，地址形如
   `https://www.xvideos.com/favorite/01234567/收藏夹名`），确认代理
   （默认 `http://127.0.0.1:10809`，留空直连）。
4. 点 **🔑 登录**，在弹出的窗口里手动完成登录（含验证码），然后直接关闭该窗口
   ——程序会自动抓取并保存 Cookie（`%APPDATA%/siterip/cookies.json`，同时生成
   yt-dlp 用的 `cookies.txt`）。
5. 点 **📥 拉取收藏列表** → 自动翻页抓取，已下载的会标绿。
6. 点 **⬇ 下载全部新增**（默认全选新增项；点击卡片可手动勾选）。

之后每次使用只需重复第 5–6 步：程序用 SQLite 记录已下载视频，只下载新增收藏。

## 界面布局

- 顶栏：登录状态、yt-dlp 状态、设置入口
- 侧栏：登录 / 拉取列表 / 下载新增 / 打开目录 / 取消队列 / 状态信息
- 主区：收藏视频卡片网格，点击勾选
- 底部：每个下载任务的进度条与实时状态

## 扩展到其它网站

后端以 `SiteAdapter` trait 抽象站点（`src/site.rs`）：

```rust
#[async_trait]
pub trait SiteAdapter {
    fn id(&self) -> &'static str;          // 站点 id
    fn home_url(&self) -> &'static str;
    fn cookie_domain(&self) -> &'static str;
    async fn list_favorites(&self, cfg, list_url, max_pages) -> Result<Vec<VideoEntry>>;
}
```

新增站点 = 在 `src/site/` 下实现该 trait + 在 `adapter_for()` 注册 + 界面站点
下拉框加一项。代理支持按站点覆盖（`config.toml` 的 `per_site_proxy`）。
下载统一交给 yt-dlp（它自带数百个站点的提取器），多数站点无需自己解析页面。

## 配置

`%APPDATA%/siterip/config.toml`：

```toml
proxy = "http://127.0.0.1:10809"
download_dir = "C:\\Users\\你\\siterip"
quality = 1080          # 画质上限，0 不限
concurrency = 2         # 同时下载任务数
favorites_url = "https://www.xvideos.com/favorite/xxxx/name"

[per_site_proxy]
# xvideos = "http://127.0.0.1:10809"   # 覆盖全局代理
```

## 借鉴自 SCrawler 的部分

- 收藏页翻页到头的判定：整页视频 ID 全部已见即停止
- 429 限速的指数退避重试
- m3u8/标题的正则提取思路（本程序实际由 yt-dlp 承担，仅在需要时参考）

## 与 SCrawler 的区别

| | SCrawler | siterip |
|---|---|---|
| 技术栈 | VB.NET WinForms + 25 站点通用框架 | Rust + Tauri，只做收藏下载 |
| 登录 | 手工塞 Cookie XML | 内嵌浏览器窗口一键抓取 |
| HLS 下载 | 串行 WebClient 逐段下载 + ffmpeg 拼接 | yt-dlp 并发分段 + 自动合并 + 续传 |
| 界面 | 巨型主窗口 | 站点卡片 + 队列进度条 |

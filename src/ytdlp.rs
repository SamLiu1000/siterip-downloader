use anyhow::{anyhow, Context, Result};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// 按行读取并**容错解码**：tokio 的 `lines()` 碰到非法 UTF-8 会直接报错，
/// 我们这边整个读取循环就静默断掉（进度没了、完成信号也收不到）。
/// 管道里哪怕混进一个坏字节也不该断流，所以用 read_until + lossy 解码。
async fn lossy_lines<R, F>(reader: R, mut on_line: F)
where
    R: tokio::io::AsyncRead + Unpin,
    F: FnMut(String),
{
    let mut buf = BufReader::new(reader);
    let mut v: Vec<u8> = Vec::new();
    loop {
        v.clear();
        match buf.read_until(b'\n', &mut v).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                while matches!(v.last(), Some(b'\n') | Some(b'\r')) {
                    v.pop();
                }
                on_line(String::from_utf8_lossy(&v).into_owned());
            }
        }
    }
}

/// 常见 Python 安装位置（资源管理器启动时 PATH 可能不含 Scripts 目录）
fn python_candidates() -> Vec<String> {
    let mut v = vec!["python".to_string()];
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        for ver in ["Python313", "Python312", "Python311", "Python310"] {
            v.push(format!("{}/Programs/Python/{}/python.exe", la, ver));
        }
    }
    v
}

fn yt_dlp_candidates() -> Vec<String> {
    let mut v = vec!["yt-dlp".to_string(), "yt-dlp.exe".to_string()];
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        for ver in ["Python313", "Python312", "Python311", "Python310"] {
            v.push(format!("{}/Programs/Python/{}/Scripts/yt-dlp.exe", la, ver));
        }
    }
    v
}

/// 定位 yt-dlp：优先 PATH 里的 yt-dlp(.exe)，其次常见 Python 路径
pub fn find_command() -> Result<Command> {
    for c in yt_dlp_candidates() {
        if which_ok(&c) {
            return Ok(mk(&c));
        }
    }
    for py in python_candidates() {
        if which_ok(&py) {
            let mut c = mk(&py);
            c.args(["-m", "yt_dlp"]);
            return Ok(c);
        }
    }
    Err(anyhow!(
        "未找到 yt-dlp。请先安装：pip install -U yt-dlp 或从 https://github.com/yt-dlp/yt-dlp/releases 下载 yt-dlp.exe 放入 PATH"
    ))
}

static VERSION_CACHE: std::sync::LazyLock<std::sync::Mutex<Option<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// 启动时后台检测版本（可能要启动子进程，避免阻塞 UI 主线程）
pub fn init_version() {
    std::thread::spawn(|| {
        // 检测整体限时 8s：Microsoft Store 的 python 桩程序可能挂起
        let (t, r) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = t.send(version().ok());
        });
        let v = r
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap_or(None);
        *VERSION_CACHE.lock().unwrap() = v;
    });
}

/// 永不阻塞：检测未完成时返回 None
pub fn cached_version() -> Option<String> {
    VERSION_CACHE.try_lock().ok().and_then(|g| g.clone())
}

/// 返回 yt-dlp 版本号
pub fn version() -> Result<String> {
    for c in yt_dlp_candidates() {
        if let Ok(o) = std::process::Command::new(&c)
            .arg("--version")
            .stdin(Stdio::null())
            .creation_flags(0x08000000)
            .output()
        {
            if o.status.success() {
                return Ok(String::from_utf8_lossy(&o.stdout).trim().to_string());
            }
        }
    }
    for py in python_candidates() {
        if let Ok(o) = std::process::Command::new(&py)
            .args(["-m", "yt_dlp", "--version"])
            .stdin(Stdio::null())
            .creation_flags(0x08000000)
            .output()
        {
            if o.status.success() {
                return Ok(String::from_utf8_lossy(&o.stdout).trim().to_string());
            }
        }
    }
    Err(anyhow!("yt-dlp 不可用"))
}

fn mk(prog: &str) -> Command {
    let mut c = Command::new(prog);
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x08000000); // CREATE_NO_WINDOW
    // stdout 是管道时 Python 会用系统 ANSI（中文 Windows = GBK）包装，
    // 输出里有 GBK 表示不了的字节就直接 OSError 22 中断下载。
    // 强制子进程用 UTF-8 输出（yt-dlp.exe 是 PyInstaller 打包的 Python，同样生效）
    c.env("PYTHONIOENCODING", "utf-8:replace")
        .env("PYTHONUTF8", "1");
    c
}

fn which_ok(prog: &str) -> bool {
    std::process::Command::new(prog)
        .arg("--version")
        .stdin(Stdio::null())
        .creation_flags(0x08000000)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub struct DownloadOptions {
    pub proxy: Option<String>,
    pub quality: u32,
    pub out_dir: PathBuf,
    pub cookies_txt: PathBuf,
    /// 站点 id：文件保存到 out_dir/<站点>/ 子目录
    pub site: String,
}

/// 是否支持浏览器指纹模拟（--impersonate，需 pip install curl_cffi）。
/// 部分站点对纯 HTTP 客户端直接回 410 Gone，模拟 Chrome 后才能正常提取。
/// 结果缓存：探测要起子进程，不能每个下载任务都跑一遍。
fn impersonate_available() -> bool {
    static CACHE: std::sync::LazyLock<std::sync::Mutex<Option<bool>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(None));
    let mut g = CACHE.lock().unwrap();
    if let Some(v) = *g {
        return v;
    }
    let (t, r) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ok = yt_dlp_candidates()
            .into_iter()
            .find(|c| which_ok(c))
            .and_then(|c| {
                std::process::Command::new(c)
                    .arg("--list-impersonate-targets")
                    .stdin(Stdio::null())
                    .creation_flags(0x08000000)
                    .output()
                    .ok()
            })
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("chrome"))
            .unwrap_or(false);
        let _ = t.send(ok);
    });
    // 探测限时 15s（首次可能冷启动 Python）
    let v = r
        .recv_timeout(std::time::Duration::from_secs(15))
        .unwrap_or(false);
    *g = Some(v);
    v
}

/// 下载单个视频，`on_line` 收到 yt-dlp 的每行输出（含进度），返回最终文件名。
/// `pause` 置 true 时杀掉 yt-dlp（已下载的 .part 保留，重跑同一命令会断点续传），
/// 调用方靠返回的 Err + 自己的标志位区分"暂停"和"失败"。
pub async fn download_video<F>(
    url: &str,
    opts: &DownloadOptions,
    pause: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut on_line: F,
) -> Result<String>
where
    F: FnMut(String),
{
    use std::sync::atomic::Ordering;
    let mut cmd = find_command()?;
    cmd.arg(url)
        .arg("--proxy")
        .arg(opts.proxy.as_deref().unwrap_or(""))
        .arg("--cookies")
        .arg(&opts.cookies_txt)
        .arg("-f")
        .arg(if opts.quality > 0 {
            format!("best[height<={}]/best", opts.quality)
        } else {
            "best".into()
        })
        .arg("-o")
        .arg(output_template(&opts.out_dir, &opts.site))
        .arg("--newline")
        .arg("--no-mtime")
        // 断点续传：保留 .part 分段，中断后从已下载处继续（yt-dlp 默认 --continue）
        .arg("--concurrent-fragments")
        .arg("4")
        .arg("--retries")
        .arg("5")
        .arg("--no-warnings");
    // 支持时始终模拟 Chrome：部分站点对裸 HTTP 客户端回 410，普通站点也不受影响
    if impersonate_available() {
        cmd.arg("--impersonate").arg("chrome");
    }

    let mut child = cmd
        .spawn()
        .context("启动 yt-dlp 失败")?;
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();

    // stdout 必须边读边回调——要是先攒进 Vec 等子进程退出再逐行调 on_line，
    // 进度就全堆在"下载完成"那一刻才出现（界面上就是"没有进度、突然已完成"）
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let o = tokio::spawn(async move {
        lossy_lines(out, |l| {
            let _ = tx.send(l);
        })
        .await;
    });
    // stderr 只在失败时展示，攒起来没关系
    let e = tokio::spawn(async move {
        let mut v = Vec::new();
        lossy_lines(err, |l| v.push(l)).await;
        v
    });

    // 边等子进程边转发进度行；每 200ms 看一眼暂停标志，置位就杀掉子进程
    // （.part 留着，恢复时重跑同一条命令断点续传）
    let mut status;
    let mut out_closed = false;
    loop {
        tokio::select! {
            st = child.wait() => { status = st?; break; }
            maybe_line = rx.recv(), if !out_closed => {
                match maybe_line {
                    Some(l) => on_line(l),
                    // 管道关了就禁用这个分支，免得 select 空转
                    None => out_closed = true,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                if pause.load(Ordering::Relaxed) {
                    let _ = child.kill().await;
                    status = child.wait().await?;
                    break;
                }
            }
        }
    }
    // 子进程结束后把管道里剩的几行也吐完（最后一段进度/文件名常在收尾时输出）
    while let Some(l) = rx.recv().await {
        on_line(l);
    }
    let _ = o.await;
    if !status.success() {
        let errs = e.await.unwrap().join("\n");
        return Err(anyhow!("yt-dlp 失败: {errs}"));
    }
    // 从进度行提取最终文件名（yt-dlp [download] Destination / has already been downloaded）
    Ok(String::new())
}

fn output_template(dir: &Path, site: &str) -> String {
    // 按站点分子目录：download_dir/<站点>/<文件>，yt-dlp 会自动创建缺失目录
    dir.join(site)
        .join("%(title).100B [%(id)s].%(ext)s")
        .to_string_lossy()
        .to_string()
}

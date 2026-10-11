window.onerror = (msg)=>{const el=document.getElementById("stats"); if(el) el.textContent="脚本错误: "+msg;};
if (!window.__TAURI__) {
  const el = document.getElementById('stats');
  if (el) el.textContent = 'Tauri API 未加载：请确认 tauri.conf.json 已开启 withGlobalTauri 并重新构建';
  throw new Error('window.__TAURI__ missing');
}
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let cfg = null;
let sites = [];            // [{id,display,home_url,favorites_url}]
let currentSite = null;
let entries = [];          // {entry:{site,id,url,title}, downloaded}
let selected = new Set();  // vid
let jobs = {};             // vid -> JobUpdate
let sessions = [];         // [{id,name,site,count}]
let activeSession = null;
let lastFetchSid = null;

const $ = (id) => document.getElementById(id);

async function init() {
  try {
  cfg = await invoke('get_state');
  sites = await invoke('list_sites');
  // 站点下拉框（列表来自后端站点注册表）
  // 目前只开放两家：其余显示为灰色不可选（后端适配尚未充分验证）
  const ENABLED_SITES = ['xvideos', 'pornhub'];
  const sel = $('site-select');
  sel.innerHTML = '';
  for (const s of sites) {
    const opt = document.createElement('option');
    opt.value = s.id;
    opt.textContent = ENABLED_SITES.includes(s.id) ? s.display : `${s.display}（暂不可用）`;
    if (!ENABLED_SITES.includes(s.id)) opt.disabled = true;
    sel.appendChild(opt);
  }
  const firstEnabled = sites.find(s => ENABLED_SITES.includes(s.id));
  sel.value = currentSite && ENABLED_SITES.includes(currentSite)
    ? currentSite
    : (firstEnabled?.id || '');
  // 地址域名是否属于该站点（镜像/子域都算）；解析失败时不拦。
  // 用于防止把 A 站地址写进 B 站配置（切换/拉取时机交错时的经典写串）
  window.urlMatchesSite = (u, siteId) => {
    const s = sites.find(x => x.id === siteId);
    if (!s || !s.home_url) return true;
    try {
      const host = new URL(u).hostname.toLowerCase().replace(/^www\./, '');
      const home = new URL(s.home_url).hostname.toLowerCase().replace(/^www\./, '');
      return host === home || host.endsWith('.' + home) || home.endsWith('.' + host);
    } catch { return true; }
  };
  // 清理历史污染：站点名下存了别站域名的地址（以前切换/拉取时机交错写串的），
  // 不清理的话该站点地址栏永远显示别站地址。清完重新拉站点列表，
  // 让无配置的站点回落到后端给的默认收藏页
  {
    let dirty = false;
    for (const s of sites) {
      const u = (cfg.config.favorites_urls || {})[s.id];
      if (u && !urlMatchesSite(u, s.id)) {
        delete cfg.config.favorites_urls[s.id];
        dirty = true;
      }
    }
    if (dirty) {
      invoke('save_config', { cfg: cfg.config }).catch(() => {});
      sites = await invoke('list_sites');
      $('stats').textContent = '已清理站点配置里不属于它的收藏地址（历史遗留的串站数据）';
    }
  }
  let prevSite = sel.value || null;
  sel.onchange = () => {
    const newSite = sel.value;
    // 用户在地址栏的未保存编辑归属"输入时的站点"：先落账再切换。
    // 否则新站点的 applyCfg 会覆盖输入，或拉取时把旧站地址写进新站名下
    const cur = $('cfg-fav-url').value.trim();
    const prevSaved = (cfg.config.favorites_urls || {})[prevSite] || '';
    if (prevSite && cur && cur !== prevSaved && urlMatchesSite(cur, prevSite)) {
      cfg.config.favorites_urls = cfg.config.favorites_urls || {};
      cfg.config.favorites_urls[prevSite] = cur;
      const s = sites.find(x => x.id === prevSite);
      if (s) s.favorites_url = cur;
      invoke('save_config', { cfg: cfg.config }).catch(() => {});
    }
    prevSite = newSite;
    currentSite = newSite;
    // 站点切换 = 换了一套收藏地址/Cookie，列表和选择都清空重来
    entries = [];
    selected.clear();
    applyCfg();
    render();
  };
  currentSite = sel.value || null;
  applyCfg();
  // 恢复上次会话持久化的抓取列表（免重新抓取；点"拉取列表"才是手动刷新）
  if (currentSite) {
    try {
      const saved = await invoke('load_entries', { cfg: cfg.config, site: currentSite });
      saved.forEach(normalizeItem);
      entries = dedupeEntries(saved);
      if (entries.length) {
        $('stats').textContent = `已恢复上次列表：${entries.length} 条（点「拉取列表」增量同步最新收藏）`;
        render();
      }
    } catch {}
  }
  await refreshSessions();
  // 标记启动恢复的列表所属的工作项目为当前项
  if (activeSession === null && sessions.length) {
    activeSession = sessions[0][0];
    renderSessions();
  }

// ---- 工作项目：列表 / 恢复 / 重命名 / 删除 ----
async function refreshSessions() {
  try { sessions = await invoke('list_sessions'); } catch { sessions = []; }
  renderSessions();
}

function renderSessions() {
  const box = $('session-list');
  box.innerHTML = '';
  if (!sessions.length) {
    box.innerHTML = '<div class="hint" style="padding:4px 10px">还没有工作项目，抓取后点「存当前」保存</div>';
    return;
  }
  for (const s of sessions) {
    const el = document.createElement('div');
    el.className = 'site-item session-item' + (s[0] === activeSession ? ' active' : '');
    el.innerHTML = `<span class="name">${escapeHtml(s[1])}</span>
      <span class="meta">${s[2] === currentSite ? '' : s[2] + ' · '}${s[3]} 条</span>
      <span class="ops">
        <button data-op="rename" title="重命名">✏</button>
        <button data-op="del" title="删除">🗑</button>
      </span>`;
    el.onclick = (ev) => {
      if (ev.target.closest('.ops')) return;
      loadSession(s[0]);
    };
    el.querySelector('[data-op=rename]').onclick = () => startRename(el, s);
    el.querySelector('[data-op=del]').onclick = (ev) => confirmDelete(ev.target, s);
    box.appendChild(el);
  }
}

async function loadSession(id) {
  try {
    const items = await invoke('load_session', { id, cfg: cfg.config });
    items.forEach(normalizeItem);
    entries = dedupeEntries(items);
    selected.clear();
    lastCheckedIdx = -1;
    activeSession = id;
    const s = sessions.find(x => x[0] === id);
    $('stats').textContent = `已恢复工作「${s ? s[1] : id}」：${entries.length} 条`;
    renderSessions();
    render();
  } catch (e) {
    $('stats').textContent = '恢复工作失败：' + e;
  }
}

// 手动保存：把当前网格内容存为一个新工作项目（默认名用本地时间，可之后重命名）
let savingSession = false;
$('btn-save-session').onclick = async () => {
  if (savingSession) return; // 防连点重复建项
  if (!entries.length) return alert('当前没有列表内容可保存（先拉取或恢复一个工作）');
  savingSession = true;
  const btn = $('btn-save-session');
  btn.disabled = true;
  btn.textContent = '保存中…';
  const d = new Date();
  const pad = (n) => String(n).padStart(2, '0');
  const name = `工作 ${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  try {
    // 幂等保存：有当前工作项目就整体更新它（点多少次也只有一个会话），
    // 没有才新建——以前每次点都新建一条，工作列表里越攒越多
    const sid = await invoke('save_session', {
      site: currentSite,
      name,
      entries: entries.map(e => e.entry),
      sid: activeSession ?? null,
    });
    activeSession = sid;
    await refreshSessions();
    $('stats').textContent = `已保存工作「${name}」：${entries.length} 条`;
  } catch (e) {
    $('stats').textContent = '保存工作失败：' + e;
  } finally {
    savingSession = false;
    btn.disabled = false;
    btn.textContent = '💾 存当前';
  }
};

// 内联重命名：名称变输入框，Enter/失焦保存，Esc 取消
function startRename(el, s) {
  const nameSpan = el.querySelector('.name');
  const input = document.createElement('input');
  input.className = 'rename';
  input.value = s[1];
  nameSpan.replaceWith(input);
  input.focus();
  input.select();
  const commit = async () => {
    const v = input.value.trim();
    if (v && v !== s[1]) {
      try { await invoke('rename_session', { id: s[0], name: v }); s[1] = v; } catch (e) { $('stats').textContent = '重命名失败：' + e; }
    }
    renderSessions();
  };
  input.onkeydown = (e) => { if (e.key === 'Enter') input.blur(); if (e.key === 'Escape') { input.value = s[1]; input.blur(); } };
  input.onblur = commit;
}

// 两步删除：第一次点变"?"，3 秒内再点才真删
function confirmDelete(btn, s) {
  if (btn.dataset.armed) {
    invoke('delete_session', { id: s[0] }).then(() => {
      if (activeSession === s[0]) { activeSession = null; entries = []; selected.clear(); render(); }
      refreshSessions();
      $('stats').textContent = `已删除工作「${s[1]}」`;
    }).catch(e => $('stats').textContent = '删除失败：' + e);
    return;
  }
  btn.dataset.armed = '1';
  btn.classList.add('confirm');
  btn.textContent = '?';
  setTimeout(() => { btn.dataset.armed = ''; btn.classList.remove('confirm'); btn.textContent = '🗑'; }, 3000);
}
  setBadge('login-badge', cfg.logged_in, cfg.logged_in ? '已登录' : '未登录');
  setBadge('ytdlp-badge', !!cfg.ytdlp_version, cfg.ytdlp_version ? `yt-dlp ${cfg.ytdlp_version}` : 'yt-dlp 检测中…');
  // yt-dlp 检测是异步的，轮询刷新徽章
  for (let i = 0; i < 6 && !cfg.ytdlp_version; i++) {
    await new Promise(r => setTimeout(r, 4000));
    cfg = await invoke('get_state');
    setBadge('ytdlp-badge', !!cfg.ytdlp_version, cfg.ytdlp_version ? `yt-dlp ${cfg.ytdlp_version}` : 'yt-dlp 未安装');
    if (cfg.ytdlp_version) break;
  }
  } catch (e) {
    const el = $('stats');
    if (el) el.textContent = '初始化失败: ' + e;
  }
}

function setBadge(id, on, text) {
  const el = $(id);
  el.textContent = text;
  el.className = 'badge ' + (on ? 'on' : 'off');
}

// 画质三档映射：最高=0 不限 / 标准=1080 上限 / 低档=720 上限（老配置值自动归档）
function qualityToKey(q) {
  if (q === 0) return 'high';
  if (q <= 720) return 'low';
  return 'standard';
}
function keyToQuality(k) {
  return k === 'high' ? 0 : (k === 'low' ? 720 : 1080);
}

function applyCfg() {
  const s = sites.find(x => x.id === currentSite);
  $('cfg-fav-url').value = (s && s.favorites_url) || '';
  $('cfg-proxy').value = cfg.config.proxy || '';
  $('cfg-dir').value = cfg.config.download_dir || '';
  $('cfg-quality').value = qualityToKey(cfg.config.quality ?? 1080);
  $('cfg-concurrency').value = String(cfg.config.concurrency ?? 2);
  $('cfg-after-done').value = cfg.config.after_done || 'none';
}

// ---- 设置 ----
$('btn-dir-pick').onclick = async () => {
  try {
    const dir = await invoke('pick_download_dir');
    if (dir) $('cfg-dir').value = dir;
  } catch (e) {
    $('stats').textContent = '选择文件夹失败：' + e;
  }
};
$('btn-settings').onclick = () => $('settings').classList.remove('hidden');
$('btn-close-settings').onclick = () => $('settings').classList.add('hidden');
$('btn-save-settings').onclick = async () => {
  cfg.config.favorites_urls = cfg.config.favorites_urls || {};
  // 地址栏的值归属当前站点；不属于当前站点的地址不落账，防止串站污染
  const favUrlNow = $('cfg-fav-url').value.trim();
  if (favUrlNow && !urlMatchesSite(favUrlNow, currentSite)) {
    $('stats').textContent = '收藏地址与当前站点不符，本次未保存该地址（其他设置已保存）';
  } else {
    cfg.config.favorites_urls[currentSite] = favUrlNow;
    const s = sites.find(x => x.id === currentSite);
    if (s) s.favorites_url = favUrlNow;
  }
  cfg.config.proxy = $('cfg-proxy').value.trim();
  cfg.config.download_dir = $('cfg-dir').value.trim();
  cfg.config.quality = keyToQuality($('cfg-quality').value);
  cfg.config.concurrency = parseInt($('cfg-concurrency').value);
  cfg.config.after_done = $('cfg-after-done').value;
  await invoke('save_config', { cfg: cfg.config });
  // 并发数立即同步给运行中的队列（没有队列时静默失败，下次启动批次自动用新值）
  invoke('set_concurrency', { n: cfg.config.concurrency }).catch(() => {});
  $('settings').classList.add('hidden');
};

// ---- 登录 ----
$('btn-login').onclick = async () => {
  if (!currentSite) return alert('没有可用站点');
  try {
    await invoke('open_login', { site: currentSite, cfg: cfg.config });
  } catch (e) {
    $('stats').textContent = '打开登录窗口失败：' + e;
    return;
  }
  $('stats').textContent = '请在弹出的窗口中登录，\n完成后关闭窗口。';
};
listen('login-done', async ({ payload }) => {
  const s = await invoke('get_state');
  setBadge('login-badge', s.logged_in, s.logged_in ? '已登录' : '未登录');
  $('stats').textContent = payload && payload.message ? payload.message : (payload ? '登录成功，Cookie 已保存。' : '未捕获到登录 Cookie');
  render();
});

// ---- 拉取收藏（增量同步：结果与已加载列表合并，不覆盖之前的工作） ----
let fetching = false;
$('btn-fetch').onclick = async () => {
  if (!currentSite) return alert('没有可用站点');
  // 地址栏输入直接生效：先把当前地址栏的值同步进配置再拉取，免去必须点"保存设置"
  const favUrl = $('cfg-fav-url').value.trim();
  if (favUrl && !urlMatchesSite(favUrl, currentSite)) {
    $('stats').textContent = '地址与当前站点不符：请确认下拉框选的是地址所属的站点，或地址粘贴错了（未保存、未拉取）';
    return;
  }
  cfg.config.favorites_urls = cfg.config.favorites_urls || {};
  if (favUrl && cfg.config.favorites_urls[currentSite] !== favUrl) {
    cfg.config.favorites_urls[currentSite] = favUrl;
    const s = sites.find(x => x.id === currentSite);
    if (s) s.favorites_url = favUrl;
    invoke('save_config', { cfg: cfg.config }).catch(() => {});
  }
  fetching = true;
  lastFetchSig = ''; // 后端已清空缓冲，签名复位让第一轮轮询必定刷新
  $('stats').textContent = '正在增量同步收藏列表（已抓取的会保留）…';
  try {
    lastFetchSid = await invoke('fetch_favorites', { site: currentSite, cfg: cfg.config });
    // 拉取不再自动创建工作项目；lastFetchSid 仅在旧后端返回正数时有意义
    if (lastFetchSid != null && lastFetchSid > 0) {
      const d = new Date();
      const pad = (n) => String(n).padStart(2, '0');
      const name = `抓取 ${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
      invoke('rename_session', { id: lastFetchSid, name }).catch(() => {});
    }
  } catch (e) {
    $('stats').textContent = '拉取失败：' + e;
  }
};
// 条目规范化 + 去重：事件与轮询双通道可能重复推送，按视频 id 只保留一条，
// 否则"总量"会虚高（事件+轮询叠加可达两倍）
function normalizeItem(it) {
  if (!it.entry) it.entry = { site: it.site, id: it.id, url: it.url, title: it.title, thumb: it.thumb };
  return it;
}
function dedupeEntries(list) {
  const m = new Map();
  for (const it of list) m.set(it.entry.id, it);
  return [...m.values()];
}
// 轮询拉取结果（仅在拉取进行中生效——否则会把恢复/加载的列表覆盖成空）
// 用"内容签名"（全部 id 拼接）判断变化：条数没变但内容变了（重新抓取同一
// 批次的更新版）也要刷新网格——以前只比条数，导致拉完网格停在旧内容
let lastFetchSig = '';
let pollTimer = setInterval(async () => {
  if (!fetching) return;
  try {
    const [items, done] = await invoke('get_favorites_so_far');
    items.forEach(normalizeItem);
    const uniq = dedupeEntries(items);
    const sig = uniq.map(x => x.entry.id).join(',');
    if (sig !== lastFetchSig) {
      lastFetchSig = sig;
      entries = uniq;
      // 不自动全选：选择交给用户（全选/框选/点击）
      $('stats').textContent = done
        ? `拉取完成：共 ${entries.length} 个视频，新增 ${entries.filter(e => !e.downloaded).length} 个`
        : `已加载 ${entries.length} 个视频（新增 ${entries.filter(e => !e.downloaded).length}）…`;
      render();
    } else if (done) {
      await finishFetch();
    }
  } catch {}
}, 2000);
listen('fav-page', ({ payload }) => {
  entries = dedupeEntries(entries.concat(payload.map(normalizeItem)));
  // 不自动全选：选择交给用户（全选/框选/点击）
  $('stats').textContent = `已加载 ${entries.length} 个视频（新增 ${entries.filter(e => !e.downloaded).length}）…`;
  render();
});
// 抓取收尾：事件通道不可靠，fav-done 与轮询都调这里，靠 fetching 标志保证只跑一次
async function finishFetch() {
  if (!fetching) return;
  fetching = false;
  lastFetchSig = '';
  try {
    const saved = await invoke('load_entries', { cfg: cfg.config, site: currentSite });
    saved.forEach(normalizeItem);
    entries = dedupeEntries(entries.concat(saved));
  } catch {}
  if (lastFetchSid != null && lastFetchSid > 0) activeSession = lastFetchSid;
  refreshSessions();
  $('stats').textContent = `同步完成：共 ${entries.length} 个，未下载 ${entries.filter(e => !e.downloaded).length} 个（需要保留就点「💾 存当前」）`;
  render();
}

listen('fav-done', () => finishFetch());
listen('fav-error', ({ payload }) => {
  fetching = false;
  $('stats').textContent = '拉取出错：' + payload;
});

// ---- 登录 Cookie 自动保存提示 ----
listen('login-progress', async ({ payload }) => {
  const s = await invoke('get_state');
  setBadge('login-badge', s.logged_in, s.logged_in ? '已登录' : '未登录');
  if (payload && payload.message) $('stats').textContent = payload.message;
});

// ---- 选择（#2 全选/反选、#3 Shift/Ctrl 连选、#5 计数） ----
let lastCheckedIdx = -1; // Shift 区间选择锚点
let listFilter = 'new';  // 'new' = 只看未下载（默认） | 'done' = 只看已下载（可勾选重下）

function selectable(raw) { return !raw.downloaded; }
// 当前过滤模式下网格里应显示的条目
function visible() {
  return entries.filter(e => listFilter === 'done' ? e.downloaded : !e.downloaded);
}

function updateSelUi() {
  const vis = visible();
  // 只数当前视图里看得见的选中项：上一批下完的条目还留在 selected 里，
  // 但它们已变成"已下载"、在未下载视图里是隐藏的——照旧计数的话，
  // 用户明明什么都没选，按钮却一直挂着"下载选中的 (1)"
  const visIds = new Set(vis.map(e => e.entry.id));
  const n = [...selected].filter(id => visIds.has(id)).length;
  const doneN = entries.filter(e => e.downloaded).length;
  $('sel-count').textContent = entries.length
    ? `已选 ${n}（共 ${entries.length}，未下载 ${entries.length - doneN}，已下载 ${doneN}）` : '';
  $('btn-download-new').textContent = `⬇ 下载选中的 (${n})`;
  // 过滤开关：点击在"未下载/已下载"两个视图间切换
  $('btn-show-done').textContent = listFilter === 'done' ? `👁 已下载 (${doneN})` : `👁 未下载 (${entries.length - doneN})`;
  $('btn-show-done').classList.toggle('active', listFilter === 'done');
  const all = $('sel-all');
  const selVis = vis.filter(e => selected.has(e.entry.id)).length;
  all.indeterminate = selVis > 0 && selVis < vis.length;
  all.checked = vis.length > 0 && selVis === vis.length;
}

// 视图切换：未下载 ⇄ 已下载（只显示已下载，方便勾选重下）
$('btn-show-done').onclick = () => {
  listFilter = listFilter === 'done' ? 'new' : 'done';
  render();
};

$('sel-all').onchange = () => {
  const on = $('sel-all').checked;
  for (const e of visible()) on ? selected.add(e.entry.id) : selected.delete(e.entry.id);
  render();
};
$('btn-invert').onclick = () => {
  for (const e of visible()) selected.has(e.entry.id) ? selected.delete(e.entry.id) : selected.add(e.entry.id);
  render();
};
$('btn-clear-sel').onclick = () => { selected.clear(); render(); };

// ---- 鼠标框选（#4）：空白处拖拽画框，相交的卡片被选中；Ctrl+拖拽为取消勾选 ----
const grid = $('grid');
let selBox = null;

grid.addEventListener('mousedown', (e) => {
  if (e.button !== 0 || e.target.closest('.card')) return; // 卡片上的点击走卡片自身逻辑
  e.preventDefault();
  selBox = { x0: e.clientX, y0: e.clientY, moved: false };
  const rect = document.createElement('div');
  rect.className = 'sel-rect';
  document.body.appendChild(rect);
  selBox.el = rect;
});
window.addEventListener('mousemove', (e) => {
  if (!selBox) return;
  if (Math.abs(e.clientX - selBox.x0) > 4 || Math.abs(e.clientY - selBox.y0) > 4) selBox.moved = true;
  if (!selBox.moved) return;
  const l = Math.min(selBox.x0, e.clientX), t = Math.min(selBox.y0, e.clientY);
  selBox.el.style.left = l + 'px';
  selBox.el.style.top = t + 'px';
  selBox.el.style.width = Math.abs(e.clientX - selBox.x0) + 'px';
  selBox.el.style.height = Math.abs(e.clientY - selBox.y0) + 'px';
});
window.addEventListener('mouseup', (e) => {
  if (!selBox) return;
  const box = selBox;
  selBox = null;
  box.el.remove();
  if (!box.moved) {
    // 空白处单击 = 清除选择（与资源管理器一致）
    if (!e.target.closest('.card') && !e.target.closest('.sel-toolbar')) { selected.clear(); render(); }
    return;
  }
  const remove = e.ctrlKey;
  const r1 = {
    l: Math.min(box.x0, e.clientX), t: Math.min(box.y0, e.clientY),
    r: Math.max(box.x0, e.clientX), b: Math.max(box.y0, e.clientY),
  };
  for (const card of grid.querySelectorAll('.card')) {
    const r2 = card.getBoundingClientRect();
    const hit = r1.l < r2.right && r1.r > r2.left && r1.t < r2.bottom && r1.b > r2.top;
    if (!hit) continue;
    const raw = entries[+card.dataset.idx];
    // 框选按当前视图取反义：未下载视图选新增，已下载视图标记重下
    if (!raw || (listFilter === 'done' ? !raw.downloaded : !selectable(raw))) continue;
    remove ? selected.delete(raw.entry.id) : selected.add(raw.entry.id);
  }
  updateSelUi();
  for (const card of grid.querySelectorAll('.card')) {
    const raw = entries[+card.dataset.idx];
    card.classList.toggle('sel', !!raw && selected.has(raw.entry.id));
  }
});

// ---- 渲染网格 ----
function render() {
  const grid = $('grid');
  grid.innerHTML = '';
  updateSelUi();
  if (!entries.length) {
    grid.innerHTML = '<div class="placeholder">暂无数据，点击「拉取收藏列表」。</div>';
    return;
  }
  if (!visible().length) {
    grid.innerHTML = `<div class="placeholder">当前视图没有条目。<br/>点上方「${listFilter === 'done' ? '👁 未下载' : '👁 已下载'}」切换视图。</div>`;
    return;
  }
  entries.forEach((raw, idx) => {
    const entry = raw.entry || raw;
    const downloaded = raw.downloaded;
    if (listFilter === 'done' ? !downloaded : downloaded) return; // 只显示当前视图的条目
    const card = document.createElement('div');
    card.className = 'card' + (selected.has(entry.id) ? ' sel' : '') + (downloaded ? ' downloaded' : '');
    card.dataset.idx = idx;
    card.innerHTML = `<div class="thumb">${
      entry.thumb
        ? `<img loading="lazy" referrerpolicy="no-referrer" src="${escapeHtml(entry.thumb)}" alt=""
             onerror="this.parentNode.textContent='▶'">`
        : '▶'
    }</div>
      <div class="t" title="${escapeHtml(entry.title)}">${escapeHtml(entry.title || entry.id)}</div>
      <div class="s ${downloaded ? 'done' : 'new'}">${downloaded ? (selected.has(entry.id) ? '↻ 待重下' : '✓ 已下载') : '● 新增'} ${jobs[entry.id] ? '· ' + jobs[entry.id].state : ''}</div>`;
    card.onmousedown = (ev) => ev.preventDefault(); // 避免拖选文字
    card.onclick = (ev) => {
      // 已下载项也可勾选 = 标记为重新下载（下载时自动清除记录后重下）
      if (ev.shiftKey && lastCheckedIdx >= 0) {
        // 区间选择：以锚点与当前项之间的所有项设为本次状态（点选时是勾选 → 区间勾选）
        const [a, b] = [Math.min(lastCheckedIdx, idx), Math.max(lastCheckedIdx, idx)];
        for (let i = a; i <= b; i++) {
          if (selectable(entries[i])) selected.add(entries[i].entry.id);
        }
      } else {
        selected.has(entry.id) ? selected.delete(entry.id) : selected.add(entry.id);
        lastCheckedIdx = idx;
      }
      updateSelUi();
      card.classList.toggle('sel', selected.has(entry.id));
    };
    grid.appendChild(card);
  });
}

function escapeHtml(s) {
  return (s || '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

// ---- 下载 ----
$('btn-download-new').onclick = async () => {
  queueNotified = false;
  const picked = entries.filter(e => selected.has(e.entry.id));
  if (!picked.length) return alert('请先选择视频（或先拉取列表）');
  // 已下载项 = 重新下载：先清掉数据库记录，否则队列会跳过
  for (const raw of picked) {
    if (raw.downloaded) {
      try {
        await invoke('forget_video', { vid: raw.entry.id, site: raw.entry.site });
        raw.downloaded = false;
      } catch (e) {
        $('stats').textContent = '清除下载记录失败：' + e;
        return;
      }
    }
    // 只初始化"没有在跑"的任务：覆盖正在下载/排队的条目会把它们的
    // 实时进度打回 queued 0%（再次点下载时进度"消失"的直接原因）
    const cur = jobs[raw.entry.id];
    if (!cur || ['done', 'failed', 'canceled'].includes(cur.state)) {
      jobs[raw.entry.id] = { id: raw.entry.id, title: raw.entry.title, state: 'queued', pct: 0, msg: '', speed: '', eta: '' };
    }
  }
  const list = picked.map(e => e.entry);
  renderJobs();
  render();
  $('btn-cancel').classList.remove('hidden');
  $('stats').textContent = `已提交 ${list.length} 个下载任务`;
  queueNotified = false; // 新一批开始，上一批的"队列完成"通知已消费，得重新武装
  try {
    await invoke('start_download', { entries: list, cfg: cfg.config });
    // 提交成功直接把任务中心弹出来：排队/进度/速度别让用户自己找入口
    $('task-center').classList.remove('hidden');
  } catch (e) {
    $('stats').textContent = '启动失败：' + e;
  }
};
$('btn-cancel').onclick = () => invoke('cancel_download');

listen('job-update', ({ payload }) => {
  applyJobUpdate(payload);
});

// 任务状态轮询：事件通道不可靠，每 1.2 秒主动拉取（有任务时才请求）
setInterval(async () => {
  if (!Object.keys(jobs).length) return;
  try {
    const list = await invoke('get_jobs');
    for (const j of list) applyJobUpdate(j);
  } catch {}
}, 1200);

let queueNotified = false;
function applyJobUpdate(payload) {
  const prev = jobs[payload.id];
  const becameDone = payload.state === 'done' && (!prev || prev.state !== 'done');
  jobs[payload.id] = payload;
  if (becameDone) {
    const e = entries.find(x => x.entry.id === payload.id);
    if (e) e.downloaded = true;
    tcSel.delete(payload.id);
    selected.delete(payload.id); // 下完了就从选择集合里清走，别留幽灵选中
    // 单任务完成即时反馈
    $('stats').textContent = `✓ 已完成：${payload.title || payload.id}`;
  }
  const hasFailed = Object.values(jobs).some(j => j.state === 'failed');
  $('btn-retry-failed').classList.toggle('hidden', !hasFailed);
  renderJobs(); // 任务中心小列表，每次轮询刷新（轻量）
  // 队列全部结束（无事件也能判定）→ 系统通知
  const vals = Object.values(jobs);
  const active = vals.filter(j => ['queued', 'running', 'paused'].includes(j.state)).length;
  if (!queueNotified && vals.length && active === 0) {
    queueNotified = true;
    const ok = vals.filter(j => j.state === 'done').length;
    const bad = vals.filter(j => j.state === 'failed').length;
    notify('下载队列完成', `成功 ${ok} 个${bad ? `，失败 ${bad} 个（可在任务中心重试）` : ''}`);
    $('btn-cancel').classList.add('hidden');
  }
  // 关键：网格全量重渲染很重（几百张卡片+缩略图），只在状态跨越时执行
  // （完成→卡片移入已下载视图；失败→卡片标记更新）
  if (becameDone || (payload.state === 'failed' && (!prev || prev.state !== 'failed'))) {
    render();
  }
}

// ---- 失败任务一键重试（#8）：只重下失败项，不动已成功的 ----
$('btn-retry-failed').onclick = () => {
  const ids = Object.values(jobs).filter(j => j.state === 'failed').map(j => j.id);
  if (!ids.length) return alert('没有可重试的失败任务');
  retryJobs(ids);
};
listen('queue-done', ({ payload }) => {
  const failed = Object.values(jobs).filter(j => j.state === 'failed').length;
  const canceled = Object.values(jobs).filter(j => j.state === 'canceled').length;
  let extra = failed ? `，失败 ${failed} 个（可重试）` : '';
  if (canceled) extra += `，取消 ${canceled} 个`;
  notify('下载队列完成', `本次成功 ${payload} 个${extra}`);
  $('btn-cancel').classList.add('hidden');
  // 用户配置的完成后动作（关机/休眠）
  const act = cfg.config.after_done;
  if (act && act !== 'none') {
    invoke('after_done_action', { action: act }).catch(e => {
      $('stats').textContent = '执行完成后动作失败：' + e;
    });
  }
});

// 系统通知（WebView2 Web Notification，失败静默降级为状态栏提示）
function notify(title, body) {
  try { new Notification(title, { body }); } catch {}
  $('stats').textContent = body;
}

// ---- 任务中心（右侧抽屉）：多选批量 + 单任务暂停/恢复/重试 ----
let tcSel = new Set();

$('btn-tc-open').onclick = () => $('task-center').classList.remove('hidden');
$('btn-close-tc').onclick = () => $('task-center').classList.add('hidden');

$('tc-sel-all').onchange = () => {
  const on = $('tc-sel-all').checked;
  for (const j of Object.values(jobs)) {
    if (j.state !== 'done') on ? tcSel.add(j.id) : tcSel.delete(j.id);
  }
  renderJobs();
};
$('btn-tc-retry').onclick = () => retryJobs([...tcSel]);
$('btn-tc-remove').onclick = () => {
  for (const id of tcSel) {
    invoke('pause_job', { id }).catch(() => {}); // 底层任务挂起不再执行
    delete jobs[id];
  }
  tcSel.clear();
  renderJobs();
};
$('btn-clear-done').onclick = () => {
  for (const [id, j] of Object.entries(jobs)) {
    if (j.state === 'done' || j.state === 'canceled') delete jobs[id];
  }
  renderJobs();
};

// 重试指定任务：清掉已下载记录（如有）后重新入队
async function retryJobs(ids) {
  queueNotified = false;
  const list = [];
  for (const id of ids) {
    const raw = entries.find(e => e.entry.id === id);
    if (!raw) continue;
    if (raw.downloaded) {
      try {
        await invoke('forget_video', { vid: id, site: raw.entry.site });
        raw.downloaded = false;
      } catch (e) {
        $('stats').textContent = '清除下载记录失败：' + e;
        return;
      }
    }
    // 同上：别覆盖还在队列里的任务状态
    const cur = jobs[id];
    if (!cur || ['done', 'failed', 'canceled'].includes(cur.state)) {
      jobs[id] = { id, title: raw.entry.title, state: 'queued', pct: 0, msg: '', speed: '', eta: '' };
    }
    list.push(raw.entry);
  }
  if (!list.length) return alert('选中的任务无法重试（本地没有对应的视频信息，请先拉取列表）');
  tcSel.clear();
  renderJobs();
  render();
  $('btn-cancel').classList.remove('hidden');
  queueNotified = false;
  try {
    await invoke('start_download', { entries: list, cfg: cfg.config });
    $('task-center').classList.remove('hidden');
  } catch (e) {
    $('stats').textContent = '启动失败：' + e;
  }
}

function renderJobs() {
  const list = Object.values(jobs);
  const btn = $('btn-tc-open');
  btn.textContent = `📋 任务中心 (${list.filter(j => j.state !== 'done').length || list.length})`;
  btn.classList.toggle('hidden', !list.length);
  const running = list.filter(j => j.state === 'running').length;
  const queued = list.filter(j => j.state === 'queued').length;
  const paused = list.filter(j => j.state === 'paused').length;
  const failed = list.filter(j => j.state === 'failed').length;
  const doneN = list.filter(j => j.state === 'done').length;
  $('job-summary').textContent = list.length
    ? `下载中 ${running} · 排队 ${queued} · 暂停 ${paused} · 失败 ${failed} · 完成 ${doneN}` : '';
  $('tc-summary').textContent = list.length
    ? `共 ${list.length} 个任务：下载中 ${running} · 排队 ${queued} · 暂停 ${paused} · 失败 ${failed} · 完成 ${doneN}（已完成的不占用队列）` : '暂无任务';
  const box = $('tc-jobs');
  box.innerHTML = '';
  for (const j of list) {
    // 完成的也显示（绿色 ✓），用户能明确看到"下完了"；用"清除已完成"手动清走
    const div = document.createElement('div');
    div.className = 'job ' + j.state;
    const speed = j.speed ? ' · ' + escapeHtml(j.speed) : '';
    const eta = j.eta ? ' · 剩余 ' + escapeHtml(j.eta) : '';
    const stTxt = j.state === 'done' ? '✓ 已完成 100%' : `${j.state} ${j.pct ? j.pct.toFixed(1) + '%' : ''}`;
    const ops = [];
    if (j.state === 'queued' || j.state === 'running') ops.push(`<button class="btn mini" data-op="pause" data-id="${j.id}" title="${j.state === 'running' ? '暂停（已下载部分保留，恢复后续传）' : '暂停'}">⏸</button>`);
    if (j.state === 'paused') ops.push(`<button class="btn mini" data-op="resume" data-id="${j.id}">▶</button>`);
    if (j.state === 'failed') ops.push(`<button class="btn mini" data-op="retry" data-id="${j.id}">↻</button>`);
    if (j.state !== 'done') ops.push(`<button class="btn mini danger" data-op="remove" data-id="${j.id}">✖</button>`);
    // 失败的完整报错单独一块：自动换行、可选中复制——状态行那点空间放不下
    // 也复制不了 yt-dlp 那种几百字的多行报错
    const errBlock = j.state === 'failed' && j.msg
      ? `<div class="job-err">
           <div class="err-text">${escapeHtml(j.msg)}</div>
           <button class="btn mini" data-op="copy-err" data-id="${j.id}" title="复制完整报错">复制</button>
         </div>`
      : '';
    // 两行式布局：第一行标题+操作按钮，第二行进度条+状态——
    // 356px 的抽屉里一行塞下"勾选+进度条+标题+按钮+速度/ETA"，标题必然被挤没
    div.innerHTML = `<input type="checkbox" class="jck" data-id="${j.id}" ${tcSel.has(j.id) ? 'checked' : ''}/>
      <div class="job-main">
        <div class="job-top">
          <div class="name" title="${escapeHtml(j.title || j.id)}">${escapeHtml(j.title || j.id)}</div>
          <div class="ops">${ops.join('')}</div>
        </div>
        <div class="job-bottom">
          <div class="bar"><i style="width:${j.state === 'done' ? 100 : j.pct}%"></i></div>
          <div class="st">${stTxt}${speed}${eta}</div>
        </div>
        ${errBlock}
      </div>`;
    box.appendChild(div);
  }
  // 操作按钮与勾选框
  box.querySelectorAll('button[data-op]').forEach(b => {
    b.onclick = () => {
      const id = b.dataset.id;
      if (b.dataset.op === 'pause') invoke('pause_job', { id }).catch(() => {});
      if (b.dataset.op === 'resume') invoke('resume_job', { id }).catch(() => {});
      if (b.dataset.op === 'retry') retryJobs([id]);
      if (b.dataset.op === 'copy-err') {
        const j = jobs[id];
        const text = j ? `${j.title || j.id}\n${j.msg || ''}` : '';
        navigator.clipboard.writeText(text).then(
          () => { b.textContent = '✓ 已复制'; setTimeout(() => { b.textContent = '复制'; }, 1500); },
          () => alert(text || '复制失败') // 剪贴板被拒时至少让用户能看到全文
        );
      }
      if (b.dataset.op === 'remove') {
        invoke('pause_job', { id }).catch(() => {});
        delete jobs[id];
        tcSel.delete(id);
      }
      setTimeout(renderJobs, 300);
    };
  });
  box.querySelectorAll('input.jck').forEach(c => {
    c.onchange = () => c.checked ? tcSel.add(c.dataset.id) : tcSel.delete(c.dataset.id);
  });
}

// ---- 其它 ----
$('btn-open-dir').onclick = () => invoke('open_download_dir', { cfg: cfg.config });
$('btn-fetch').disabled = false;

init();

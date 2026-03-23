use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum::body::Body;
use bytes::Bytes;
use moka::future::Cache;
use oxipng::{optimize_from_memory, Options, StripChunks};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use serde::{Deserialize, Serialize};
use std::io::Cursor; // 删除了 Write
// 删除了 UdpSocket
use std::path::Path as StdPath;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;

// --- 数据结构 ---

struct AppState {
    cache: Cache<String, Bytes>,           // 图片内容缓存
    xml_cache: Cache<String, String>,      // 注入后的 XML 缓存
    structured_epg: Cache<String, Vec<EpgProgram>>, // 结构化频道 JSON 缓存
    server_port: u16,                      // 运行时端口
}

#[derive(Serialize, Clone)]
struct EpgProgram {
    start: String,
    stop: String,
    title: String,
    desc: String,
}

#[derive(Deserialize)]
struct ClearCacheRequest {
    target: String,
}

// --- 核心业务逻辑 ---

async fn fetch_and_process_epg(host: String) -> Result<String, String> {
    let remote_url = "https://epg.iill.top/epg.xml";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let raw_xml = client.get(remote_url).send().await
        .map_err(|e| e.to_string())?
        .text().await
        .map_err(|e| e.to_string())?;

    let mut reader = Reader::from_str(&raw_xml);
    let mut writer = Writer::new(Cursor::new(Vec::new()));
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"channel" => {
                let id = e.attributes().flatten()
                    .find(|a| a.key.as_ref() == b"id")
                    .map(|a| String::from_utf8_lossy(&a.value).into_owned())
                    .unwrap_or_default();

                writer.write_event(Event::Start(e.clone())).unwrap();
                if !id.is_empty() {
                    let logo_url = format!("http://{}/logo/{}.png", host, id);
                    let mut icon_tag = BytesStart::new("icon");
                    icon_tag.push_attribute(("src", logo_url.as_str()));
                    writer.write_event(Event::Empty(icon_tag)).unwrap();
                }
            }
            Ok(Event::Eof) => break,
            Ok(event) => { writer.write_event(event).map_err(|e| e.to_string())?; }
            Err(e) => return Err(format!("XML Parse Error: {}", e)),
        }
        buf.clear();
    }
    Ok(String::from_utf8_lossy(&writer.into_inner().into_inner()).into_owned())
}

async fn refresh_structured_cache(state: Arc<AppState>, xml: &str) {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut buf = Vec::new();
    let mut current_channel = String::new();
    let mut temp_map: std::collections::HashMap<String, Vec<EpgProgram>> = std::collections::HashMap::new();
    let mut current_prog: Option<EpgProgram> = None;
    let mut current_tag = String::new();

    while let Ok(event) = reader.read_event_into(&mut buf) {
        match event {
            Event::Start(ref e) if e.name().as_ref() == b"programme" => {
                current_channel = e.attributes().flatten().find(|a| a.key.as_ref() == b"channel").map(|a| String::from_utf8_lossy(&a.value).into_owned()).unwrap_or_default();
                current_prog = Some(EpgProgram {
                    start: e.attributes().flatten().find(|a| a.key.as_ref() == b"start").map(|a| String::from_utf8_lossy(&a.value).into_owned()).unwrap_or_default(),
                    stop: e.attributes().flatten().find(|a| a.key.as_ref() == b"stop").map(|a| String::from_utf8_lossy(&a.value).into_owned()).unwrap_or_default(),
                    title: "".into(), desc: "".into(),
                });
            }
            Event::Start(ref e) => { current_tag = String::from_utf8_lossy(e.name().as_ref()).to_string(); }
            Event::Text(ref e) => {
                if let Some(ref mut p) = current_prog {
                    let text = e.unescape().unwrap_or_else(|_| String::from_utf8_lossy(e.as_ref()).into()).trim().to_string();
                    if !text.is_empty() {
                        match current_tag.as_str() { "title" => p.title = text, "desc" => p.desc = text, _ => {} }
                    }
                }
            }
            Event::CData(ref e) => {
                if let Some(ref mut p) = current_prog {
                    let text = String::from_utf8_lossy(e.as_ref()).trim().to_string();
                    if !text.is_empty() {
                        match current_tag.as_str() { "title" => p.title = text, "desc" => p.desc = text, _ => {} }
                    }
                }
            }
            Event::End(ref e) if e.name().as_ref() == b"programme" => {
                if let Some(p) = current_prog.take() {
                    if !p.title.is_empty() { temp_map.entry(current_channel.clone()).or_insert_with(Vec::new).push(p); }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    for (cid, progs) in temp_map { state.structured_epg.insert(cid, progs).await; }
}

// --- 路由 ---

async fn get_epg_xml(headers: header::HeaderMap, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let host = get_host(&headers, state.server_port);
    if let Some(cached) = state.xml_cache.get(&host).await { return ([(header::CONTENT_TYPE, "application/xml")], cached).into_response(); }
    match fetch_and_process_epg(host.clone()).await {
        Ok(xml) => {
            state.xml_cache.insert(host, xml.clone()).await;
            let s_clone = Arc::clone(&state); let x_clone = xml.clone();
            tokio::spawn(async move { refresh_structured_cache(s_clone, &x_clone).await; });
            ([(header::CONTENT_TYPE, "application/xml")], xml).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_channel_epg(Path(ch): Path<String>, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(p) = state.structured_epg.get(&ch).await { return Json(p).into_response(); }
    let host = format!("127.0.0.1:{}", state.server_port);
    if let Ok(xml) = fetch_and_process_epg(host).await {
        refresh_structured_cache(Arc::clone(&state), &xml).await;
        if let Some(p) = state.structured_epg.get(&ch).await { return Json(p).into_response(); }
    }
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

async fn clear_cache_handler(State(state): State<Arc<AppState>>, Json(payload): Json<ClearCacheRequest>) -> impl IntoResponse {
    match payload.target.as_str() {
        "xml" => { state.xml_cache.invalidate_all(); state.structured_epg.invalidate_all(); }
        "logo" => state.cache.invalidate_all(),
        "all" => { state.xml_cache.invalidate_all(); state.cache.invalidate_all(); state.structured_epg.invalidate_all(); }
        _ => return (StatusCode::BAD_REQUEST, "Invalid target").into_response(),
    }
    (StatusCode::OK, "Cache Cleared").into_response()
}

async fn delete_logo(Path(name): Path<String>, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let path = StdPath::new("./icons").join(&name);
    let _ = fs::remove_file(path).await;
    state.cache.invalidate(&name).await;
    StatusCode::OK
}

fn get_host(h: &header::HeaderMap, port: u16) -> String {
    h.get("host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("127.0.0.1:{}", port))
}

async fn get_logo(Path(n): Path<String>, State(s): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(d) = s.cache.get(&n).await { return build_image_response(d); }
    if let Ok(d) = fs::read(StdPath::new("./icons").join(&n)).await {
        let b = Bytes::from(d); s.cache.insert(n, b.clone()).await; return build_image_response(b);
    }
    Err(StatusCode::NOT_FOUND)
}

fn build_image_response(d: Bytes) -> Result<Response<Body>, StatusCode> {
    Response::builder()
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=604800, immutable")
        .body(Body::from(d))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn upload_handler(State(state): State<Arc<AppState>>, mut multipart: Multipart) -> impl IntoResponse {
    while let Ok(Some(field)) = multipart.next_field().await {
        if let Some(name) = field.file_name().map(|s| s.to_string()) {
            if !name.to_lowercase().ends_with(".png") { continue; }
            if let Ok(raw) = field.bytes().await {
                let compressed = tokio::task::spawn_blocking(move || {
                    let mut opt = Options::from_preset(2); opt.strip = StripChunks::Safe;
                    optimize_from_memory(&raw, &opt).ok().map(Bytes::from)
                }).await.unwrap();
                if let Some(data) = compressed {
                    state.cache.insert(name.clone(), data.clone()).await;
                    let _ = fs::write(StdPath::new("./icons").join(&name), data).await;
                }
            }
        }
    }
    StatusCode::OK
}

async fn list_icons() -> impl IntoResponse {
    let mut files = Vec::new();
    if let Ok(mut entries) = fs::read_dir("./icons").await {
        while let Ok(Some(e)) = entries.next_entry().await { files.push(e.file_name().to_string_lossy().to_string()); }
    }
    files.sort(); Json(files)
}

#[tokio::main]
async fn main() {
    let _ = fs::create_dir_all("./icons").await;

    // 动态获取端口
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("PORT 必须是有效数字");

    let cache = Cache::builder().max_capacity(500).time_to_idle(Duration::from_secs(43200)).build();
    let xml_cache = Cache::builder().max_capacity(10).time_to_live(Duration::from_secs(3600)).build();
    let structured_epg = Cache::builder().max_capacity(2000).time_to_live(Duration::from_secs(3600)).build();

    let state = Arc::new(AppState {
        cache,
        xml_cache,
        structured_epg,
        server_port: port
    });

    let app = Router::new()
        .route("/admin", get(admin_page))
        .route("/admin/clear_cache", post(clear_cache_handler))
        .route("/admin/delete_logo/:name", post(delete_logo))
        .route("/list", get(list_icons))
        .route("/logo/:name", get(get_logo))
        .route("/upload", post(upload_handler))
        .route("/epg.xml", get(get_epg_xml))
        .route("/epg/:channel", get(get_channel_epg))
        .with_state(state)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive());

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("🚀 Server running on http://{}", addr);
    axum::serve(listener, app).await.unwrap();
}

async fn admin_page() -> Html<String> {
    Html(r##"
<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>EPG 控制台</title>
    <style>
        body { font-family: system-ui, sans-serif; max-width: 1100px; margin: 0 auto; padding: 20px; background: #f0f2f5; color: #333; }
        .card { background: white; padding: 20px; border-radius: 12px; box-shadow: 0 2px 10px rgba(0,0,0,0.05); margin-bottom: 20px; }
        .btn { padding: 8px 16px; border: none; border-radius: 6px; cursor: pointer; font-weight: 500; transition: 0.2s; font-size: 14px; }
        .btn-blue { background: #007bff; color: white; }
        .btn-red { background: #ff4d4f; color: white; }
        .btn-outline { border: 1px solid #ddd; background: white; color: #666; }
        .btn:hover { opacity: 0.8; }
        .upload-zone { border: 2px dashed #007bff; padding: 40px; text-align: center; border-radius: 8px; cursor: pointer; background: #f8fbff; color: #007bff; }
        .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(130px, 1fr)); gap: 15px; }
        .item { background: #fff; border: 1px solid #eee; padding: 12px; border-radius: 10px; text-align: center; position: relative; transition: 0.2s; }
        .item:hover { transform: translateY(-2px); box-shadow: 0 4px 12px rgba(0,0,0,0.1); }
        .item img { width: 100%; height: 90px; object-fit: contain; background: #fafafa; border-radius: 4px; cursor: zoom-in; }
        .item .del { position: absolute; top: -5px; right: -5px; background: #ff4d4f; color: white; width: 22px; height: 22px; border-radius: 50%; line-height: 20px; font-weight: bold; cursor: pointer; display: none; box-shadow: 0 2px 4px rgba(0,0,0,0.2); }
        .item:hover .del { display: block; }
        .name { font-size: 12px; color: #444; word-break: break-all; margin: 8px 0; display: block; height: 2.8em; overflow: hidden; }
        .actions { display: flex; gap: 4px; margin-top: 5px; }
        .actions button, .actions a { flex: 1; font-size: 10px; padding: 4px 0; text-decoration: none; border: 1px solid #eee; border-radius: 4px; background: #fff; color: #007bff; cursor: pointer; text-align: center; }
        .actions a:hover, .actions button:hover { background: #f0f7ff; }
        #modal { display: none; position: fixed; top: 0; left: 0; width: 100%; height: 100%; background: rgba(0,0,0,0.85); z-index: 9999; justify-content: center; align-items: center; backdrop-filter: blur(5px); }
        #modal img { max-width: 90%; max-height: 85%; object-fit: contain; background: white; padding: 10px; border-radius: 8px; }
    </style>
</head>
<body>
    <h2>🛠️ EPG 图标管理系统</h2>

    <div class="card">
        <h3>🚀 系统维护</h3>
        <div style="display: flex; gap: 12px;">
            <button class="btn btn-outline" onclick="clearCache('xml')">🔄 刷新远程节目单</button>
            <button class="btn btn-outline" onclick="clearCache('logo')">🖼️ 清空图片缓存</button>
            <button class="btn btn-red" onclick="clearCache('all')">🔥 全部重置</button>
        </div>
    </div>

    <div class="card">
        <h3>📤 上传 PNG 图标 (ID.png)</h3>
        <div class="upload-zone" id="drop-zone">点击此处或拖拽 PNG 文件上传</div>
        <input type="file" id="file-input" multiple accept="image/png" style="display: none;">
    </div>

    <div class="card">
        <input type="text" id="search" placeholder="🔍 输入频道 ID 快速过滤..." style="width:100%; padding:12px; border:1px solid #ddd; border-radius:8px; margin-bottom:20px; font-size: 15px;">
        <div class="grid" id="grid">加载中...</div>
    </div>

    <div id="modal" onclick="this.style.display='none'"><img id="modal-img"></div>

    <script>
        const grid = document.getElementById('grid');
        const search = document.getElementById('search');
        let files = [];

        // 兼容性剪贴板函数
        function copyToClipboard(text) {
            if (navigator.clipboard && window.isSecureContext) {
                navigator.clipboard.writeText(text).then(() => alert('已复制地址'));
            } else {
                const input = document.createElement('input');
                input.value = text;
                document.body.appendChild(input);
                input.select();
                try {
                    document.execCommand('copy');
                    alert('已复制地址');
                } catch (err) {
                    alert('复制失败，请手动复制');
                }
                document.body.removeChild(input);
            }
        }

        async function clearCache(target) {
            if(!confirm(`确定要清理 ${target} 缓存吗？`)) return;
            const res = await fetch('/admin/clear_cache', {
                method: 'POST',
                headers: {'Content-Type': 'application/json'},
                body: JSON.stringify({ target })
            });
            if(res.ok) {
                alert('缓存已清理');
                if(target !== 'xml') refresh();
            }
        }

        async function deleteIcon(name) {
            if(!confirm(`确定要从硬盘永久删除 ${name} 吗？`)) return;
            await fetch(`/admin/delete_logo/${name}`, { method: 'POST' });
            refresh();
        }

        function preview(src) {
            document.getElementById('modal-img').src = src;
            document.getElementById('modal').style.display = 'flex';
        }

        async function refresh() {
            try {
                const res = await fetch('/list');
                files = await res.json();
                render();
            } catch (e) { grid.innerText = "获取列表失败"; }
        }

        function render() {
            const q = search.value.toLowerCase();
            const filtered = files.filter(f => f.toLowerCase().includes(q));
            grid.innerHTML = filtered.map(f => {
                const id = f.split('.')[0];
                const fullUrl = `${window.location.origin}/logo/${f}`;
                return `
                    <div class="item">
                        <div class="del" onclick="deleteIcon('${f}')">×</div>
                        <img src="${fullUrl}?t=${Date.now()}" onclick="preview('${fullUrl}')">
                        <span class="name">${f}</span>
                        <div class="actions">
                            <a href="/epg/${id}" target="_blank">节目</a>
                            <button onclick="copyToClipboard('${fullUrl}')">复制</button>
                        </div>
                    </div>
                `;
            }).join('');
        }

        // 获取拖拽区域元素
        const dropZone = document.getElementById('drop-zone');
        const fileInput = document.getElementById('file-input');

        // 1. 阻止浏览器默认行为（防止拖进去直接打开图片）
        ['dragenter', 'dragover', 'dragleave', 'drop'].forEach(eventName => {
            dropZone.addEventListener(eventName, e => {
                e.preventDefault();
                e.stopPropagation();
            }, false);
        });

        // 2. 拖拽悬停视觉效果
        ['dragenter', 'dragover'].forEach(eventName => {
            dropZone.addEventListener(eventName, () => {
                dropZone.style.background = '#e1efff';
                dropZone.style.borderColor = '#0056b3';
            }, false);
        });

        ['dragleave', 'drop'].forEach(eventName => {
            dropZone.addEventListener(eventName, () => {
                dropZone.style.background = '#f8fbff';
                dropZone.style.borderColor = '#007bff';
            }, false);
        });

        // 3. 处理文件丢入
        dropZone.addEventListener('drop', e => {
            const dt = e.dataTransfer;
            const files = dt.files;
            handleUpload(files); // 调用统一的上传函数
        }, false);

        // 4. 处理点击上传
        fileInput.onchange = (e) => handleUpload(e.target.files);
        dropZone.onclick = () => fileInput.click();

        // 5. 统一上传逻辑
        async function handleUpload(files) {
            if (files.length === 0) return;

            const fd = new FormData();
            let hasValidFile = false;

            for (let f of files) {
                if (f.type === "image/png" || f.name.toLowerCase().endsWith('.png')) {
                    fd.append('f', f);
                    hasValidFile = true;
                }
            }

            if (!hasValidFile) {
                alert("请上传 PNG 格式的图片");
                return;
            }

            const dz = document.getElementById('drop-zone');
            const originalText = dz.innerText;
            dz.innerText = "⏳ 正在上传并优化...";

            try {
                const res = await fetch('/upload', { method: 'POST', body: fd });
                if (res.ok) {
                    console.log("上传成功");
                }
            } catch (err) {
                alert("上传失败: " + err);
            } finally {
                dz.innerText = originalText;
                refresh(); // 刷新列表
            }
        }

        document.getElementById('drop-zone').onclick = () => document.getElementById('file-input').click();
        document.getElementById('file-input').onchange = async (e) => {
            const fd = new FormData();
            for(let f of e.target.files) fd.append('f', f);
            const dz = document.getElementById('drop-zone');
            dz.innerText = "⏳ 正在上传并优化...";
            await fetch('/upload', { method: 'POST', body: fd });
            dz.innerText = "点击此处或拖拽 PNG 文件上传";
            refresh();
        };

        search.oninput = render;
        refresh();
    </script>
</body>
</html>
"##.to_string())
}
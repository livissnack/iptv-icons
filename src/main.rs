use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response, Html},
    routing::{get, post},
    Router,
    body::Body,
};
use std::sync::Arc;
use bytes::Bytes;
use std::path::Path as StdPath;
use tokio::fs;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use oxipng::{optimize_from_memory, Options, StripChunks};
use moka::future::Cache;
use std::time::Duration;
use std::net::UdpSocket;

struct AppState {
    cache: Cache<String, Bytes>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("PORT 必须是一个有效的数字");

    let cache = Cache::builder()
        .max_capacity(500)
        .time_to_idle(Duration::from_secs(43200))
        .build();

    let shared_state = Arc::new(AppState { cache });

    warm_up_cache(Arc::clone(&shared_state), "./icons").await;

    let app = Router::new()
        .route("/health", get(|| async { "OK" }))
        .route("/admin", get(admin_page))
        .route("/list", get(list_icons))
        .route("/logo/:name", get(get_logo))
        .route("/upload", post(upload_handler))
        .with_state(shared_state)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive());

    let local_ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();

    println!("\n🚀 EPG 图标管理系统已就绪！");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  📡 本地地址: http://localhost:{}", port);
    println!("  🌐 网络地址: http://{}:{}", local_ip, port);
    println!("  🎨 管理后台: http://{}:{}/admin", local_ip, port);
    println!("  📦 容量上限: 500 个图标 (LRU 模式)");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    axum::serve(listener, app).await.unwrap();
}

async fn get_logo(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Some(data) = state.cache.get(&name).await {
        return build_image_response(data);
    }

    let path = StdPath::new("./icons").join(&name);
    if let Ok(data) = fs::read(&path).await {
        let bytes = Bytes::from(data);
        state.cache.insert(name, bytes.clone()).await;
        return build_image_response(bytes);
    }

    Err(StatusCode::NOT_FOUND)
}

fn build_image_response(data: Bytes) -> Result<Response<Body>, StatusCode> {
    let res = Response::builder()
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=604800, immutable")
        .body(Body::from(data))
        .unwrap();
    Ok(res)
}

async fn upload_handler(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut file_count = 0;
    if let Ok(mut entries) = fs::read_dir("./icons").await {
        while let Ok(Some(_)) = entries.next_entry().await { file_count += 1; }
    }

    let mut uploaded_count = 0;
    while let Ok(Some(field)) = multipart.next_field().await {
        if file_count >= 500 {
            return (StatusCode::BAD_REQUEST, "已达到 500 个图标上限，请删除后再上传").into_response();
        }

        if let Some(raw_name) = field.file_name().map(|s| s.to_string()) {
            let safe_name = StdPath::new(&raw_name)
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());

            let Some(file_name) = safe_name else { continue };
            if !file_name.to_lowercase().ends_with(".png") { continue; }

            if let Ok(raw_data) = field.bytes().await {
                let compressed = tokio::task::spawn_blocking(move || {
                    let mut options = Options::from_preset(2);
                    options.strip = StripChunks::Safe;
                    optimize_from_memory(&raw_data, &options).ok().map(Bytes::from)
                }).await.unwrap();

                if let Some(data) = compressed {
                    state.cache.insert(file_name.clone(), data.clone()).await;
                    let _ = fs::create_dir_all("./icons").await;
                    let _ = fs::write(StdPath::new("./icons").join(&file_name), data).await;
                    uploaded_count += 1;
                    file_count += 1;
                }
            }
        }
    }
    (StatusCode::OK, format!("成功处理 {} 个图标", uploaded_count)).into_response()
}

async fn list_icons() -> impl IntoResponse {
    let mut files = Vec::new();
    if let Ok(mut entries) = fs::read_dir("./icons").await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            files.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    files.sort();
    axum::Json(files)
}

async fn warm_up_cache(state: Arc<AppState>, dir_path: &str) {
    if let Ok(mut entries) = fs::read_dir(dir_path).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.path().is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Ok(data) = fs::read(entry.path()).await {
                    state.cache.insert(name, Bytes::from(data)).await;
                }
            }
        }
    }
}

fn get_local_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip().to_string())
}

async fn admin_page() -> Html<String> {
    Html(r##"
<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>EPG 图标管理</title>
    <style>
        * { box-sizing: border-box; }
        body { font-family: system-ui, sans-serif; max-width: 1000px; margin: 0 auto; padding: 15px; background: #f4f4f9; color: #333; }
        .card { background: white; padding: 15px; border-radius: 12px; box-shadow: 0 4px 12px rgba(0,0,0,0.05); margin-bottom: 15px; }
        .upload-area { border: 2px dashed #007bff; padding: 30px; text-align: center; cursor: pointer; border-radius: 8px; background: #f8fbff; transition: 0.2s; }
        .upload-area:hover { background: #f0f7ff; }
        #search { width: 100%; padding: 12px; margin: 10px 0 20px 0; border: 1px solid #ddd; border-radius: 8px; font-size: 16px; outline: none; }
        .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(110px, 1fr)); gap: 12px; }
        @media (max-width: 480px) { .grid { grid-template-columns: repeat(2, 1fr); } }
        .item { background: #fff; border: 1px solid #eee; padding: 12px; text-align: center; border-radius: 10px; cursor: pointer; }
        .item img { width: 100%; aspect-ratio: 1/1; object-fit: contain; margin-bottom: 8px; background: #fdfdfd; border-radius: 4px; }
        .name { font-size: 12px; color: #666; word-break: break-all; line-height: 1.2; height: 2.4em; overflow: hidden; display: block; }
        #modal { display: none; position: fixed; top: 0; left: 0; width: 100%; height: 100%; background: rgba(0,0,0,0.8); z-index: 1000; justify-content: center; align-items: center; backdrop-filter: blur(4px); }
        #modal img { max-width: 90%; max-height: 80%; object-fit: contain; background: #fff; border-radius: 8px; padding: 10px; }
    </style>
</head>
<body>
    <h2 style="text-align: center;">🎨 EPG 图标管理后台</h2>
    <div class="card">
        <div class="upload-area" id="drop-zone">📤 点击或拖拽上传 (自动压缩)</div>
        <input type="file" id="file-input" multiple accept="image/png" style="display: none;">
    </div>
    <div class="card">
        <input type="text" id="search" placeholder="🔍 搜索图标 (上限 500 个)...">
        <div class="grid" id="grid">加载中...</div>
    </div>
    <div id="modal" onclick="this.style.display='none'"><img id="modal-img"></div>
    <script>
        const grid = document.getElementById('grid');
        const search = document.getElementById('search');
        let files = [];

        async function refresh() {
            try {
                const res = await fetch('/list');
                files = await res.json();
                render();
            } catch (e) { grid.innerText = "加载失败"; }
        }
        function render() {
            const q = search.value.toLowerCase();
            const filtered = files.filter(f => f.toLowerCase().includes(q));
            grid.innerHTML = '';
            filtered.forEach(f => {
                const div = document.createElement('div');
                // window.location.origin 会自动获取如 http://1.2.3.4:3000 这样的基础地址
                const fullUrl = `${window.location.origin}/logo/${f}`; 
                
                div.className = 'item';
                // 直接设置 title 为完整 URL，方便用户悬停预览
                div.title = fullUrl; 
                
                div.onclick = () => { 
                    document.getElementById('modal-img').src = `/logo/${f}`; 
                    document.getElementById('modal').style.display = 'flex'; 
                };
                
                div.innerHTML = `<img src="/logo/${f}?t=${Date.now()}" loading="lazy"><span class="name"></span>`;
                div.querySelector('.name').textContent = f;
                grid.appendChild(div);
            });
        }
        async function upload(fileList) {
            if (fileList.length === 0) return;
            const fd = new FormData();
            for (let f of fileList) fd.append('f', f);
            const dz = document.getElementById('drop-zone');
            dz.innerText = "⏳ 正在压缩上传...";
            const res = await fetch('/upload', { method: 'POST', body: fd });
            if (!res.ok) alert(await res.text());
            dz.innerText = "📤 点击或拖拽上传 (自动压缩)";
            refresh();
        }
        document.getElementById('drop-zone').onclick = () => document.getElementById('file-input').click();
        document.getElementById('file-input').onchange = (e) => upload(e.target.files);
        search.oninput = render;
        refresh();
    </script>
</body>
</html>
"##.to_string())
}
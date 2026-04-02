use std::collections::HashMap;
use dashmap::DashMap;
use std::sync::OnceLock;
use axum::{
    extract::{Query, Host},
    response::{IntoResponse, Response},
    http::{StatusCode, header},
    body::Body,
};
use serde_json::Value;

// 缓存重写后的 M3U8 内容
static M3U8_CACHE: OnceLock<DashMap<String, (String, u64)>> = OnceLock::new();

fn get_cache() -> &'static DashMap<String, (String, u64)> {
    M3U8_CACHE.get_or_init(DashMap::new)
}

pub async fn cjyun_handler(
    Query(params): Query<HashMap<String, String>>,
    Host(host): Host,
) -> impl IntoResponse {
    // --- 步骤 3: 响应代理后的 TS 切片请求 ---
    if let Some(ts_url) = params.get("ts") {
        return proxy_ts(ts_url).await.into_response();
    }

    let id = params.get("id").map(|s| s.as_str()).unwrap_or("");
    if id.is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing id").into_response();
    }

    println!("[CJYUN] Processing request for ID: {}", id);

    // --- 步骤 1: 从 API 获取原始 stream 地址 ---
    let parts: Vec<&str> = id.split('_').collect();
    if parts.len() != 2 {
        println!("[CJYUN] Error: Invalid ID format: {}", id);
        return (StatusCode::BAD_REQUEST, "Invalid ID format").into_response();
    }
    let site_id = parts[0];
    let channel_id = parts[1];

    let raw_m3u8_url = match get_api_stream_url(site_id, channel_id).await {
        Some(url) => url,
        None => {
            println!("[CJYUN] Error: Could not get play_url from API for site: {}, ch: {}", site_id, channel_id);
            return (StatusCode::NOT_FOUND, "API returned no stream").into_response();
        }
    };

    println!("[CJYUN] Found raw M3U8 URL: {}", raw_m3u8_url);

    // --- 步骤 2: 获取 M3U8 内容并重写切片地址 ---
    match fetch_and_rebuild_m3u8(&raw_m3u8_url, &host, site_id).await {
        Some(m3u8_content) => {
            println!("[CJYUN] Successfully rebuilt M3U8 for site: {}", site_id);
            (
                [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
                m3u8_content,
            ).into_response()
        },
        None => {
            println!("[CJYUN] Error: Failed to fetch or rebuild M3U8 for {}", raw_m3u8_url);
            (StatusCode::FORBIDDEN, "Failed to fetch or rewrite M3U8").into_response()
        },
    }
}

/// 1. 请求 API 获取播放地址
async fn get_api_stream_url(site_id: &str, channel_id: &str) -> Option<String> {
    let client = reqwest::Client::new();
    let api_url = format!("https://app.cjyun.org/video/player/streamlist?site_id={}&live_type=1", site_id);

    let resp = client.get(api_url)
        .header("Referer", "http://app.cjyun.org/")
        .send().await.ok()?;

    if !resp.status().is_success() {
        println!("[API] HTTP Error: {}", resp.status());
        return None;
    }

    let json: Value = resp.json().await.ok()?;
    let list = json["data"].as_array()?;

    for item in list {
        let cur_id = match &item["id"] {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => item["id"].to_string().replace('"', ""),
        };
        if cur_id == channel_id {
            return item["play_url"].as_str().or(item["url"].as_str()).map(|s| s.to_string());
        }
    }
    None
}

/// 2. 伪装请求头获取 M3U8 并重写
async fn fetch_and_rebuild_m3u8(url: &str, host: &str, site_id: &str) -> Option<String> {
    let client = reqwest::Client::new();

    // 长江云不同子站可能对 Referer 要求不同，这里统一尝试使用该站点的根域名或主域名
    let resp = client.get(url)
        .header("Referer", "http://app.cjyun.org/")
        .header("User-Agent", "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1")
        .send().await.ok()?;

    if !resp.status().is_success() {
        println!("[M3U8] Source returned error: {} for URL: {}", resp.status(), url);
        return None;
    }

    let body = resp.text().await.ok()?;
    if !body.contains("#EXTM3U") {
        println!("[M3U8] Error: Response is not a valid M3U8 file. Body length: {}", body.len());
        if body.len() < 500 { println!("[M3U8] Body: {}", body); }
        return None;
    }

    // 处理 Base URL，去除查询参数以确保路径拼接正确
    let base_path = url.split('?').next().unwrap_or(url);
    let base_url = if let Some(pos) = base_path.rfind('/') {
        &base_path[..pos]
    } else {
        base_path
    };

    let mut lines = Vec::new();
    let mut ts_count = 0;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }

        if line.starts_with('#') {
            lines.push(line.to_string());
        } else {
            // 拼接完整 TS 路径
            let full_ts = if line.starts_with("http") {
                line.to_string()
            } else if line.starts_with('/') {
                // 处理绝对路径
                let domain = url.split('/').take(3).collect::<Vec<_>>().join("/");
                format!("{}{}", domain, line)
            } else {
                // 处理相对路径
                format!("{}/{}", base_url, line)
            };

            // 代理 TS 地址
            let proxied = format!("http://{}/cjyun?id={}&ts={}", host, site_id, urlencoding::encode(&full_ts));
            lines.push(proxied);
            ts_count += 1;
        }
    }

    println!("[M3U8] Rebuilt done. Replaced {} TS segments.", ts_count);
    Some(lines.join("\n"))
}

/// 3. TS 切片代理请求
async fn proxy_ts(ts_url: &str) -> Response {
    let client = reqwest::Client::new();
    // 代理请求必须带上 Referer 否则切片会 403
    let resp = client.get(ts_url)
        .header("Referer", "http://app.cjyun.org/")
        .header("User-Agent", "Mozilla/5.0")
        .send().await;

    match resp {
        Ok(res) if res.status().is_success() => {
            let data = res.bytes().await.unwrap_or_default();
            Response::builder()
                .header(header::CONTENT_TYPE, "video/mp2t")
                .header(header::CACHE_CONTROL, "public, max-age=3600")
                .body(Body::from(data))
                .unwrap()
                .into_response()
        }
        Ok(res) => {
            println!("[TS] Proxy Failed: {} for URL: {}", res.status(), ts_url);
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(e) => {
            println!("[TS] Request Error: {}", e);
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

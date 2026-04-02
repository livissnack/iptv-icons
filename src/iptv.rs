use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use dashmap::DashMap;
use std::sync::OnceLock;
use base64::{Engine as _, engine::general_purpose};
use axum::{
    extract::{Path, Query},
    response::{Redirect, IntoResponse},
    http::StatusCode,
};

static HUYA_CACHE: OnceLock<DashMap<String, (String, u64)>> = OnceLock::new();

fn get_cache() -> &'static DashMap<String, (String, u64)> {
    HUYA_CACHE.get_or_init(DashMap::new)
}

fn get_huya_url(room_id: &str, cdn: &str, format: &str) -> Option<String> {
    let client = reqwest::blocking::Client::new();
    let url = format!("https://mp.huya.com/cache1.php?m=Live&do=profileRoom&roomid={}", room_id);

    let resp: serde_json::Value = client.get(url).send().ok()?.json().ok()?;
    let stream_info = &resp["data"]["stream"]["baseSteamInfoList"][0];

    let stream_name = stream_info["sStreamName"].as_str()?;
    let flv_anti_code = stream_info["sFlvAntiCode"].as_str().unwrap_or("");

    let mut anti_map = HashMap::new();
    for pair in flv_anti_code.split('&') {
        let parts: Vec<&str> = pair.splitn(2, '=').collect();
        if parts.len() == 2 {
            anti_map.insert(parts[0], parts[1]);
        }
    }

    let uid = "1560173900";
    let stream_type = "102";
    let ctype = "tars_wap";
    let now_dur = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let ws_time = format!("{:x}", now_dur.as_secs() + 3600);
    let seq_id = format!("{}", now_dur.as_millis());

    let hash_input = format!("{}|{}|{}", seq_id, ctype, stream_type);
    let hash = format!("{:x}", md5::compute(hash_input));

    let fm_encoded = anti_map.get("fm")?;
    let fm_urldecoded = urlencoding::decode(fm_encoded).ok()?;
    let fm_bytes = general_purpose::STANDARD.decode(fm_urldecoded.as_ref()).ok()?;
    let fm = String::from_utf8(fm_bytes).ok()?;

    let ws_secret_input = fm
        .replace("$0", uid)
        .replace("$1", stream_name)
        .replace("$2", &hash)
        .replace("$3", &ws_time);
    let ws_secret = format!("{:x}", md5::compute(ws_secret_input));

    let ext = if format == "hls" { "m3u8" } else { "flv" };
    let domain = if format == "hls" { "hls" } else { "flv" };
    let fs = anti_map.get("fs").unwrap_or(&"");

    Some(format!(
        "https://{}.{}.huya.com/src/{}.{}?wsSecret={}&wsTime={}&ctype={}&seqid={}&uid={}&fs={}&ver=1&t={}",
        cdn, domain, stream_name, ext, ws_secret, ws_time, ctype, seq_id, uid, fs, stream_type
    ))
}

pub async fn huya_handler(
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let cdn = params.get("cdn").cloned().unwrap_or_else(|| "al".to_string());
    let format = params.get("format").cloned().unwrap_or_else(|| "hls".to_string());

    let cache_key = format!("{}:{}:{}", id, cdn, format);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let cache = get_cache();

    if let Some(entry) = cache.get(&cache_key) {
        let (url, expire_at): &(String, u64) = entry.value();
        if now < *expire_at {
            return Redirect::temporary(url).into_response();
        }
    }

    let id_c = id.clone();
    let cdn_c = cdn.clone();
    let format_c = format.clone();

    let result = tokio::task::spawn_blocking(move || {
        get_huya_url(&id_c, &cdn_c, &format_c)
    }).await.ok().flatten();

    match result {
        Some(url) => {
            cache.insert(cache_key, (url.clone(), now + 1800));
            Redirect::temporary(&url).into_response()
        }
        None => (StatusCode::NOT_FOUND, "Huya stream not found").into_response(),
    }
}
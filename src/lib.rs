mod common;
mod config;
mod proxy;

use crate::config::Config;
use crate::proxy::*;

use std::collections::HashMap;
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;
use worker::*;
use once_cell::sync::Lazy;
use regex::Regex;
use rand::{seq::SliceRandom, thread_rng};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

static PROXYIP_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[\w-]+-\d+$").unwrap());
static CC_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"^([A-Z]{2})(\d+)?$").unwrap());
static MULTI_CC_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"^([A-Z]{2}-)+[A-Z]{2}$").unwrap());

struct ProxyCache {
    list: Vec<(String, u16, String, String)>,
    last_updated: u64,
}

static PROXY_CACHE: Lazy<Mutex<ProxyCache>> = Lazy::new(|| {
    Mutex::new(ProxyCache {
        list: Vec::new(),
        last_updated: 0,
    })
});

#[event(fetch)]
async fn main(req: Request, env: Env, _: Context) -> Result<Response> {
    let uuid = env
        .var("UUID")
        .map(|x| Uuid::parse_str(&x.to_string()).unwrap_or_default())?;
    let host = req.url()?.host().map(|x| x.to_string()).unwrap_or_default();
    let main_page_url = env.var("MAIN_PAGE_URL").map(|x|x.to_string()).unwrap();
    let sub_page_url = env.var("SUB_PAGE_URL").map(|x|x.to_string()).unwrap();
    let config = Config { uuid, host: host.clone(), proxy_addr: host, proxy_port: 443, main_page_url, sub_page_url};

    Router::with_data(config)
        .on_async("/", fe)
        .on_async("/sub", sub)
        .on("/link", link)
        .on_async("/Free/TG-at-BitzBlack/:path", handle_proxy_requests)
        .run(req, env)
        .await
}

async fn get_response_from_url(url: String) -> Result<Response> {
    let req = Fetch::Url(Url::parse(url.as_str())?);
    let mut res = req.send().await?;
    Response::from_html(res.text().await?)
}

async fn fe(_: Request, cx: RouteContext<Config>) -> Result<Response> {
    get_response_from_url(cx.data.main_page_url).await
}

async fn sub(_: Request, cx: RouteContext<Config>) -> Result<Response> {
    get_response_from_url(cx.data.sub_page_url).await
}

async fn handle_proxy_requests(req: Request, cx: RouteContext<Config>) -> Result<Response> {
    let path = cx.param("path").unwrap_or_default();
    let upgrade = req.headers().get("Upgrade")?.unwrap_or_default();

    if upgrade.eq_ignore_ascii_case("websocket") {
        handle_websocket(req, cx, path).await
    } else {
        handle_http_proxy_request(path).await
    }
}

async fn handle_websocket(req: Request, cx: RouteContext<Config>, path: String) -> Result<Response> {
    let mut proxyip = path.clone();
    
    if path.len() == 2 {
        if let Some(entry) = get_cc_proxy(&path, 0).await?.first() {
            proxyip = format!("{}-{}", entry.0, entry.1);
        }
    }

    if PROXYIP_PATTERN.is_match(&proxyip) {
        if let Some((addr, port)) = parse_proxyip(&proxyip) {
            cx.data.proxy_addr = addr;
            cx.data.proxy_port = port;
        }
    }

    let WebSocketPair { server, client } = WebSocketPair::new()?;
    server.accept()?;

    wasm_bindgen_futures::spawn_local(async move {
        let events = server.events().unwrap();
        if let Err(e) = ProxyStream::new(cx.data, &server, events).process().await {
            console_log!("[tunnel]: {}", e);
        }
    });

    Response::from_websocket(client)
}

async fn handle_http_proxy_request(path: String) -> Result<Response> {
    if path == "ip-port" {
        return Response::ok("Direct IP-Port request");
    }

    if let Some((cc, index)) = parse_cc_pattern(&path) {
        return get_cc_proxy(&cc, index).await
            .and_then(|proxies| pick_proxy(proxies, index))
    }

    if MULTI_CC_PATTERN.is_match(&path) {
        let ccs: Vec<&str> = path.split('-').collect();
        return get_multi_cc_proxy(ccs).await
            .and_then(|proxies| pick_proxy(proxies, 0))
    }

    Response::error("Invalid request", 400)
}

fn parse_cc_pattern(path: &str) -> Option<(String, usize)> {
    CC_PATTERN.captures(path)
        .map(|cap| {
            let cc = cap.get(1).unwrap().as_str().to_string();
            let index = cap.get(2)
                .map(|m| m.as_str().parse().unwrap_or(0))
                .unwrap_or(0);
            (cc, index)
        })
}

async fn get_cc_proxy(cc: &str, index: usize) -> Result<Vec<(String, u16, String, String)>> {
    let cache = get_proxy_list().await?;
    let filtered: Vec<_> = cache.iter()
        .filter(|(_, _, c, _)| c == cc)
        .cloned()
        .collect();

    if filtered.is_empty() {
        return Err(Error::from("No proxies found"));
    }

    Ok(filtered)
}

async fn get_multi_cc_proxy(ccs: Vec<&str>) -> Result<Vec<(String, u16, String, String)>> {
    let cache = get_proxy_list().await?;
    let filtered: Vec<_> = cache.iter()
        .filter(|(_, _, c, _)| ccs.contains(&c.as_str()))
        .cloned()
        .collect();

    if filtered.is_empty() {
        return Err(Error::from("No proxies found"));
    }

    Ok(filtered)
}

fn pick_proxy(proxies: Vec<(String, u16, String, String)>, index: usize) -> Result<Response> {
    if index > 0 && index <= proxies.len() {
        let (ip, port, _, _) = &proxies[index-1];
        return Response::ok(format!("{}:{}", ip, port));
    }

    let mut rng = thread_rng();
    let (ip, port, _, _) = proxies.choose(&mut rng)
        .ok_or(Error::from("Selection failed"))?;
    
    Response::ok(format!("{}:{}", ip, port))
}

async fn get_proxy_list() -> Result<Vec<(String, u16, String, String)>> {
    let mut cache = PROXY_CACHE.lock().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    if now - cache.last_updated < 60 && !cache.list.is_empty() {
        return Ok(cache.list.clone());
    }

    let url = "https://raw.githubusercontent.com/FoolVPN-ID/Nautica/refs/heads/main/kvProxyList.json";
    let req = Fetch::Url(Url::parse(url)?);
    let mut res = req.send().await?;

    if res.status_code() != 200 {
        return Err(Error::from("Failed to fetch proxy list"));
    }

    let data: HashMap<String, Vec<String>> = serde_json::from_str(&res.text().await?)?;
    let mut list = Vec::new();

    for (cc, entries) in data {
        for entry in entries {
            let parts: Vec<&str> = entry.split(':').collect();
            if parts.len() == 2 {
                let ip = parts[0].to_string();
                let port = parts[1].parse::<u16>().unwrap_or(0);
                list.push((ip, port, cc.clone(), "Unknown".to_string()));
            }
        }
    }

    cache.list = list.clone();
    cache.last_updated = now;
    Ok(list)
}

fn parse_proxyip(proxyip: &str) -> Option<(String, u16)> {
    proxyip.split_once('-')
        .and_then(|(addr, port)| {
            port.parse::<u16>().ok()
                .map(|p| (addr.to_string(), p))
        })
}

fn link(_: Request, cx: RouteContext<Config>) -> Result<Response> {
    #[derive(Serialize)]
    struct Link {
        links: [String; 4],
    }

    let host = cx.data.host.to_string();
    let uuid = cx.data.uuid.to_string();

    let vmess_link = {
        let config = json!({
            "ps": "siren vmess",
            "v": "2",
            "add": host.clone(),
            "port": "80",
            "id": uuid.clone(),
            "aid": "0",
            "scy": "zero",
            "net": "ws",
            "type": "none",
            "host": host.clone(),
            "path": "/Free/TG-at-BitzBlack/KR",
            "tls": "",
            "sni": "",
            "alpn": ""
        });
        format!("vmess://{}", URL_SAFE.encode(config.to_string()))
    };
    
    let vless_link = format!(
        "vless://{uuid}@{host}:443?encryption=none&type=ws&host={host}&path=%2FFree%2FTG-at-BitzBlack%2FKR&security=tls&sni={host}#siren vless"
    );
    
    let trojan_link = format!(
        "trojan://{uuid}@{host}:443?encryption=none&type=ws&host={host}&path=%2FFree%2FTG-at-BitzBlack%2FKR&security=tls&sni={host}#siren trojan"
    );
    
    let ss_link = format!(
        "ss://{}@{host}:443?plugin=v2ray-plugin%3Btls%3Bmux%3D0%3Bmode%3Dwebsocket%3Bpath%3D%2FFree%2FTG-at-BitzBlack%2FKR%3Bhost%3D{host}#siren ss",
        URL_SAFE.encode(format!("none:{uuid}"))
    );

    Response::from_json(&Link {
        links: [vmess_link, vless_link, trojan_link, ss_link],
    })
    }

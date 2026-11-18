//! The official mobile-SMS page handles its own security verification.
//! Only its one-time ticket is returned to a loopback server; no SMS/password is collected here.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use quark_protocol::{Session, SmsLogin, SmsLoginConfig};
use tokio::sync::oneshot;
use url::Url;

#[derive(Clone)]
struct Bridge {
    nonce: String,
    origin: String,
    frame_url: String,
    frame_origin: String,
    ticket: Arc<Mutex<Option<oneshot::Sender<String>>>>,
}

fn router(bridge: Bridge) -> Router {
    Router::new()
        .route("/login/{nonce}", get(page))
        .route("/complete/{nonce}", post(complete))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(bridge)
}

async fn page(State(bridge): State<Bridge>, Path(nonce): Path<String>) -> Response {
    if nonce != bridge.nonce {
        return StatusCode::NOT_FOUND.into_response();
    }
    match render_page(&bridge) {
        Ok(html) => (
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::REFERRER_POLICY, "no-referrer"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            Html(html),
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn complete(
    State(bridge): State<Bridge>,
    Path(nonce): Path<String>,
    headers: HeaderMap,
    Json(value): Json<serde_json::Value>,
) -> StatusCode {
    if nonce != bridge.nonce {
        return StatusCode::NOT_FOUND;
    }
    let origin = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok());
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    if origin != Some(bridge.origin.as_str()) || host != bridge.origin.strip_prefix("http://") {
        return StatusCode::FORBIDDEN;
    }
    let Some(ticket) = value.get("ticket").and_then(|v| v.as_str()) else {
        return StatusCode::BAD_REQUEST;
    };
    if ticket.len() != 32 || !ticket.bytes().all(|b| b.is_ascii_graphic()) {
        return StatusCode::BAD_REQUEST;
    }
    let Ok(mut slot) = bridge.ticket.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    match slot.take() {
        Some(sender) => match sender.send(ticket.to_owned()) {
            Ok(()) => StatusCode::OK,
            Err(_) => StatusCode::GONE,
        },
        None => StatusCode::CONFLICT,
    }
}

fn render_page(bridge: &Bridge) -> Result<String> {
    let frame_url = serde_json::to_string(&bridge.frame_url)?;
    let origin = serde_json::to_string(&bridge.frame_origin)?;
    let endpoint = serde_json::to_string(&format!("/complete/{}", bridge.nonce))?;
    Ok(format!(
        r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>夸克网盘 · 手机验证码登录</title>
<style>body{{font:16px system-ui;background:#f4f6fb;margin:0;padding:24px;text-align:center;color:#202530}}main{{max-width:460px;margin:auto;background:white;border-radius:16px;padding:24px}}iframe{{width:100%;height:520px;border:0}}p{{line-height:1.6}}#status{{font-size:14px;color:#555}}</style>
<main><h2>手机号和短信验证码登录</h2><p>请在下方夸克官方页面完成安全验证和短信登录。</p><iframe id="official" title="夸克官方短信登录" referrerpolicy="no-referrer"></iframe><p id="status">本页面只接收官方登录结果，不接收或保存手机号和短信验证码。</p></main>
<script>
const frame=document.getElementById('official');frame.src={frame_url};
let submitted=false;
window.addEventListener('message',async(event)=>{{
 if(event.origin!=={origin}||event.source!==frame.contentWindow||typeof event.data!=='string'||event.data.length!==32||submitted)return;
 submitted=true;
 const status=document.getElementById('status');status.textContent='短信校验已完成，正在返回终端…';
 try{{const response=await fetch({endpoint},{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{ticket:event.data}})}});
  if(!response.ok)throw new Error('callback');status.textContent='已收到官方登录结果，请查看终端的会话验证结果。';
 }}catch(error){{status.textContent='无法返回终端：请确认登录命令仍在运行，然后重新登录。';submitted=false;}}
}});
</script></html>"#
    ))
}

pub async fn login(config: SmsLoginConfig, port: u16) -> Result<Session> {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .context("无法启动本地短信登录页面，请更换 --port")?;
    let address = listener.local_addr()?;
    let origin = format!("http://{address}");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let mut url = Url::parse(&config.login_url)?;
    url.query_pairs_mut().append_pair("origin", &origin);
    let (sender, receiver) = oneshot::channel();
    let bridge = Bridge {
        nonce: nonce.clone(),
        origin: origin.clone(),
        frame_origin: url.origin().ascii_serialization(),
        frame_url: url.into(),
        ticket: Arc::new(Mutex::new(Some(sender))),
    };
    let (stop_sender, stop_receiver) = oneshot::channel();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, router(bridge))
            .with_graceful_shutdown(async {
                let _ = stop_receiver.await;
            })
            .await
    });
    eprintln!("请打开本机短信登录页面：\n{origin}/login/{nonce}");
    eprintln!(
        "SSH 远程使用：先转发端口 {} 到服务器的 127.0.0.1:{}，再在本机打开上述地址。",
        address.port(),
        address.port()
    );
    eprintln!(
        "仅支持手机号和短信验证码；页面安全验证由夸克官方处理。有效期 10 分钟，Ctrl+C 取消。"
    );
    let result: Result<String> = tokio::select! {
        ticket = receiver => ticket.context("登录页面已关闭"),
        signal = tokio::signal::ctrl_c() => { signal?; Err(anyhow::anyhow!("已取消登录")) },
        _ = tokio::time::sleep(Duration::from_secs(600)) => Err(anyhow::anyhow!("短信登录等待超时")),
        result = &mut server => Err(anyhow::anyhow!("本地登录服务提前结束：{result:?}")),
    };
    let _ = stop_sender.send(());
    if !server.is_finished()
        && tokio::time::timeout(Duration::from_secs(2), &mut server)
            .await
            .is_err()
    {
        server.abort();
        let _ = server.await;
    }
    let ticket = result?;
    tokio::select! {
        result = SmsLogin::finish_official_sms(config, &ticket) => Ok(result?),
        signal = tokio::signal::ctrl_c() => { signal?; bail!("已取消登录"); },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;

    #[tokio::test]
    async fn callback_checks_origin_nonce_ticket_and_replay() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let (tx, rx) = oneshot::channel();
        let bridge = Bridge {
            nonce: "test-nonce".into(),
            origin: origin.clone(),
            frame_url: SmsLoginConfig::default().login_url,
            frame_origin: "https://uop.quark.cn".into(),
            ticket: Arc::new(Mutex::new(Some(tx))),
        };
        let task = tokio::spawn(async move {
            axum::serve(listener, router(bridge)).await.unwrap();
        });
        let client = Client::new();
        let url = format!("{origin}/complete/test-nonce");
        let body = serde_json::json!({"ticket":"12345678901234567890123456789012"});
        assert_eq!(
            client
                .post(&url)
                .header("Origin", "https://unrelated.example")
                .json(&body)
                .send()
                .await?
                .status(),
            403
        );
        assert_eq!(
            client
                .post(format!("{origin}/complete/wrong-nonce"))
                .header("Origin", &origin)
                .json(&body)
                .send()
                .await?
                .status(),
            404
        );
        assert_eq!(
            client
                .post(&url)
                .header("Origin", &origin)
                .json(&serde_json::json!({"ticket":"short"}))
                .send()
                .await?
                .status(),
            400
        );
        assert_eq!(
            client
                .post(&url)
                .header("Origin", &origin)
                .json(&body)
                .send()
                .await?
                .status(),
            200
        );
        assert_eq!(rx.await?, "12345678901234567890123456789012");
        assert_eq!(
            client
                .post(&url)
                .header("Origin", &origin)
                .json(&body)
                .send()
                .await?
                .status(),
            409
        );
        task.abort();
        Ok(())
    }

    #[test]
    fn page_only_embeds_official_mobile_flow_and_validates_message_source() -> Result<()> {
        let (tx, _rx) = oneshot::channel();
        let bridge = Bridge {
            nonce: "nonce".into(),
            origin: "http://127.0.0.1:1234".into(),
            frame_url: SmsLoginConfig::default().login_url,
            frame_origin: "https://uop.quark.cn".into(),
            ticket: Arc::new(Mutex::new(Some(tx))),
        };
        let html = render_page(&bridge)?;
        assert!(html.contains("custom_login_type=mobile"));
        assert!(html.contains("event.source!==frame.contentWindow"));
        assert!(html.contains("event.origin!==\"https://uop.quark.cn\""));
        assert!(!html.contains("custom_login_type=common"));
        Ok(())
    }
}

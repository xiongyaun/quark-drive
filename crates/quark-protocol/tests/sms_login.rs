use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    Arc, Mutex,
};

use axum::extract::{Form, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use quark_protocol::{
    ClientConfig, QuarkClient, SmsLogin, SmsLoginConfig, SmsSendOutcome, DEFAULT_SORT,
};
use serde_json::json;

const TICKET: &str = "12345678901234567890123456789012";

#[derive(Default)]
struct Control {
    captcha: AtomicBool,
    missing_cookie: AtomicBool,
    redirect_send: AtomicBool,
    send_status: AtomicI64,
    commit_status: AtomicI64,
    sent: AtomicUsize,
    commits: AtomicUsize,
    exchanges: AtomicUsize,
    redirected: AtomicUsize,
    forms: Mutex<Vec<BTreeMap<String, String>>>,
}
struct Server {
    config: SmsLoginConfig,
    state: Arc<Control>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start() -> Self {
        let state = Arc::new(Control::default());
        state.send_status.store(2_000_000, Ordering::SeqCst);
        state.commit_status.store(20_000, Ordering::SeqCst);
        let app = Router::new()
            .route("/login", get(bootstrap))
            .route("/send", post(send))
            .route("/commit", post(commit))
            .route("/account/info", get(account))
            .route("/steal", get(steal))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let config = SmsLoginConfig {
            login_url: format!("{base}/login"),
            send_url: format!("{base}/send"),
            commit_url: format!("{base}/commit"),
            account_info_url: format!("{base}/account/info"),
            timeout_seconds: 5,
        };
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            config,
            state,
            task,
        }
    }
}
async fn bootstrap(State(c): State<Arc<Control>>) -> Html<String> {
    let captcha = if c.captcha.load(Ordering::SeqCst) {
        "<input name='scene_v2' value='official-scene'>"
    } else {
        ""
    };
    Html(format!("<html><form id='u_form'><input name='client_id' value='532'><input name='display' value='pc'><input name='key' value='a&amp;b=1'><input name='login_name'><input name='sms_code' maxlength='4'>{captcha}</form></html>"))
}
async fn send(
    State(c): State<Arc<Control>>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    c.sent.fetch_add(1, Ordering::SeqCst);
    c.forms.lock().unwrap().push(form);
    if c.redirect_send.load(Ordering::SeqCst) {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/steal")],
        )
            .into_response();
    }
    Json(json!({"status":c.send_status.load(Ordering::SeqCst),"message":"send result"}))
        .into_response()
}
async fn commit(
    State(c): State<Arc<Control>>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Json<serde_json::Value> {
    c.commits.fetch_add(1, Ordering::SeqCst);
    c.forms.lock().unwrap().push(form);
    Json(
        json!({"status":c.commit_status.load(Ordering::SeqCst),"message":"invalid SMS code","data":TICKET}),
    )
}
async fn account(
    State(c): State<Arc<Control>>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    c.exchanges.fetch_add(1, Ordering::SeqCst);
    if query.get("st").map(String::as_str) != Some(TICKET)
        || query.get("lw").map(String::as_str) != Some("mobile")
    {
        return (StatusCode::BAD_REQUEST, Json(json!({"success":false}))).into_response();
    }
    let body = Json(json!({"success":true,"data":{"uid":"fake-sms-user"}}));
    if c.missing_cookie.load(Ordering::SeqCst) {
        body.into_response()
    } else {
        (
            [(header::SET_COOKIE, "__pus=sms-session; Path=/; HttpOnly")],
            body,
        )
            .into_response()
    }
}
async fn steal(State(c): State<Arc<Control>>) -> &'static str {
    c.redirected.fetch_add(1, Ordering::SeqCst);
    "not allowed"
}

#[tokio::test]
async fn sms_protocol_preserves_form_and_establishes_usable_cloud_session() {
    let s = Server::start().await;
    let mut login = SmsLogin::start(s.config.clone()).await.unwrap();
    assert_eq!(
        login.send_code("+8613800000000").await.unwrap(),
        SmsSendOutcome::Sent
    );
    let session = login.verify_code("1234").await.unwrap();
    assert_eq!(session.get("__pus"), Some("sms-session"));
    let forms = s.state.forms.lock().unwrap().clone();
    assert_eq!(forms[0].get("client_id").unwrap(), "532");
    assert_eq!(forms[0].get("key").unwrap(), "a&b=1");
    assert_eq!(forms[0].get("login_name").unwrap(), "13800000000");
    assert!(!forms[0].contains_key("sms_code"));
    assert_eq!(forms[1].get("sms_code").unwrap(), "1234");
    assert_eq!(s.state.exchanges.load(Ordering::SeqCst), 1);
    // The saved session must work with the existing cloud client, not only parse.
    let cloud = quark_testkit::FakeQuarkServer::start().await;
    let client = QuarkClient::new(
        session,
        ClientConfig {
            api_base: cloud.api_base(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        client
            .list_dir("0", 1, 100, DEFAULT_SORT)
            .await
            .unwrap()
            .total,
        2
    );
}

#[tokio::test]
async fn official_security_verification_is_not_skipped() {
    let s = Server::start().await;
    s.state.captcha.store(true, Ordering::SeqCst);
    let mut login = SmsLogin::start(s.config.clone()).await.unwrap();
    assert_eq!(
        login.send_code("13800000000").await.unwrap(),
        SmsSendOutcome::BrowserVerificationRequired
    );
    assert_eq!(s.state.sent.load(Ordering::SeqCst), 0);
    assert!(login.verify_code("1234").await.is_err());
}

#[tokio::test]
async fn server_challenge_requires_official_page_and_is_not_retried() {
    let s = Server::start().await;
    s.state.send_status.store(20_180_001, Ordering::SeqCst);
    let mut login = SmsLogin::start(s.config.clone()).await.unwrap();
    assert_eq!(
        login.send_code("13800000000").await.unwrap(),
        SmsSendOutcome::BrowserVerificationRequired
    );
    assert_eq!(s.state.sent.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bad_code_never_exchanges_or_returns_session() {
    let s = Server::start().await;
    s.state.commit_status.store(20_001, Ordering::SeqCst);
    let mut login = SmsLogin::start(s.config.clone()).await.unwrap();
    login.send_code("13800000000").await.unwrap();
    assert!(login.verify_code("1234").await.is_err());
    assert_eq!(s.state.exchanges.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ticket_exchange_requires_real_auth_cookie() {
    let s = Server::start().await;
    s.state.missing_cookie.store(true, Ordering::SeqCst);
    assert!(SmsLogin::finish_official_sms(s.config.clone(), TICKET)
        .await
        .is_err());
}

#[tokio::test]
async fn official_page_ticket_can_establish_a_session() {
    let s = Server::start().await;
    assert!(SmsLogin::finish_official_sms(s.config.clone(), "short")
        .await
        .is_err());
    assert_eq!(s.state.exchanges.load(Ordering::SeqCst), 0);
    let session = SmsLogin::finish_official_sms(s.config.clone(), TICKET)
        .await
        .unwrap();
    assert!(session.is_authenticated());
}

#[tokio::test]
async fn authentication_post_does_not_follow_redirects() {
    let s = Server::start().await;
    s.state.redirect_send.store(true, Ordering::SeqCst);
    let mut login = SmsLogin::start(s.config.clone()).await.unwrap();
    assert!(login.send_code("13800000000").await.is_err());
    assert_eq!(s.state.redirected.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invalid_phone_and_code_are_rejected_locally() {
    let s = Server::start().await;
    let mut login = SmsLogin::start(s.config.clone()).await.unwrap();
    assert!(login.send_code("not a phone").await.is_err());
    assert!(login.verify_code("1234").await.is_err());
    login.send_code("13800000000").await.unwrap();
    assert!(login.verify_code("12xx").await.is_err());
    assert_eq!(s.state.commits.load(Ordering::SeqCst), 0);
}

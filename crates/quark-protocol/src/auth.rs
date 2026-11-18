//! Official web SMS protocol. Android SecurityGuard payloads are not reused.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{ACCEPT, ORIGIN, REFERER};
use reqwest::redirect::Policy;
use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use crate::{Error, Result, Session};

#[derive(Clone)]
pub struct SmsLoginConfig {
    pub login_url: String,
    pub send_url: String,
    pub commit_url: String,
    pub account_info_url: String,
    pub timeout_seconds: u64,
}

impl Default for SmsLoginConfig {
    fn default() -> Self {
        Self {
            login_url: "https://uop.quark.cn/cas/custom/login?custom_login_type=mobile&client_id=532&display=pc".into(),
            send_url: "https://uop.quark.cn/cas/custom/login/sendSmsCode?custom_login_type=mobile".into(),
            commit_url: "https://uop.quark.cn/cas/custom/login/commit?custom_login_type=mobile".into(),
            account_info_url: "https://pan.quark.cn/account/info".into(),
            timeout_seconds: 20,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SmsSendOutcome {
    Sent,
    /// Complete the genuine official-page security verification. Never synthesize its token.
    BrowserVerificationRequired,
}

pub struct SmsLogin {
    http: reqwest::Client,
    jar: Arc<Jar>,
    config: SmsLoginConfig,
    fields: BTreeMap<String, String>,
    phone: Option<String>,
    code_sent: bool,
    code_length: usize,
}

impl SmsLogin {
    fn new(config: SmsLoginConfig) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .cookie_provider(jar.clone())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(config.timeout_seconds))
            // Authentication tickets and form values must not follow arbitrary redirects.
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            http,
            jar,
            config,
            fields: BTreeMap::new(),
            phone: None,
            code_sent: false,
            code_length: 4,
        })
    }

    pub async fn start(config: SmsLoginConfig) -> Result<Self> {
        let mut login = Self::new(config)?;
        let response = login.http.get(&login.config.login_url).send().await?;
        if !response.status().is_success() {
            return Err(Error::Other("无法加载官方短信登录页".into()));
        }
        let html = response.text().await?;
        let (fields, length) = parse_bootstrap(&html)?;
        login.fields = fields;
        login.code_length = length;
        Ok(login)
    }

    pub fn normalize_phone(phone: &str) -> Result<String> {
        let phone = phone.trim();
        let phone = phone.strip_prefix("+86").unwrap_or(phone);
        if phone.len() != 11
            || !phone.starts_with('1')
            || !phone.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(Error::Other(
                "该官方短信入口要求中国大陆 11 位手机号码".into(),
            ));
        }
        Ok(phone.to_owned())
    }

    pub async fn send_code(&mut self, phone: &str) -> Result<SmsSendOutcome> {
        let phone = Self::normalize_phone(phone)?;
        // The shipped page obtains these proofs through the real CAPTCHA SDK.
        // Do not submit empty or fabricated proof when the page requires it.
        if ["scene_v2", "nc_appkey", "captcha_id"]
            .iter()
            .any(|key| self.fields.get(*key).is_some_and(|s| !s.is_empty()))
        {
            return Ok(SmsSendOutcome::BrowserVerificationRequired);
        }
        self.code_sent = false;
        let mut form = self.fields.clone();
        form.insert("login_name".into(), phone.clone());
        form.remove("sms_code");
        let value = self.post(&self.config.send_url, &form).await?;
        if status(&value) == Some(2_000_000) {
            self.phone = Some(phone);
            self.code_sent = true;
            return Ok(SmsSendOutcome::Sent);
        }
        if status(&value) == Some(20_180_001)
            || value.pointer("/data/captchaImage").is_some()
            || value.pointer("/data/captchaId").is_some()
        {
            return Ok(SmsSendOutcome::BrowserVerificationRequired);
        }
        Err(api_error(&value, "短信发送未成功"))
    }

    pub async fn verify_code(&mut self, code: &str) -> Result<Session> {
        if !self.code_sent {
            return Err(Error::Other("尚未成功发送短信验证码".into()));
        }
        let code = code.trim();
        if code.len() != self.code_length || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Other(format!(
                "短信验证码应为 {} 位数字",
                self.code_length
            )));
        }
        let mut form = self.fields.clone();
        form.insert(
            "login_name".into(),
            self.phone.clone().ok_or(Error::AuthRequired)?,
        );
        form.insert("sms_code".into(), code.to_owned());
        let value = self.post(&self.config.commit_url, &form).await?;
        if status(&value) != Some(20_000) {
            return Err(api_error(&value, "短信验证码校验失败"));
        }
        let ticket = value
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Other("短信登录响应缺少服务票据".into()))?;
        let session = self.exchange(ticket).await?;
        self.code_sent = false;
        Ok(session)
    }

    /// Called only after the official mobile-SMS page returns its ticket.
    pub async fn finish_official_sms(config: SmsLoginConfig, ticket: &str) -> Result<Session> {
        Self::new(config)?.exchange(ticket).await
    }

    async fn post(&self, url: &str, fields: &BTreeMap<String, String>) -> Result<Value> {
        let response = self
            .http
            .post(url)
            .header(
                ORIGIN,
                Url::parse(&self.config.login_url)?
                    .origin()
                    .ascii_serialization(),
            )
            .header(REFERER, &self.config.login_url)
            .header(ACCEPT, "application/json")
            .header("X-Requested-With", "XMLHttpRequest")
            .form(fields)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::Other(format!(
                "短信登录请求失败：HTTP {}",
                response.status()
            )));
        }
        Ok(response.json().await?)
    }

    async fn exchange(&self, ticket: &str) -> Result<Session> {
        if ticket.len() != 32 || !ticket.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Error::Other("官方短信页面返回的服务票据格式无效".into()));
        }
        let mut url = Url::parse(&self.config.account_info_url)?;
        url.query_pairs_mut()
            .append_pair("st", ticket)
            .append_pair("lw", "mobile");
        let response = self
            .http
            .get(url.clone())
            .header(REFERER, "https://pan.quark.cn/")
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::Other(format!(
                "建立网盘会话失败：HTTP {}",
                response.status()
            )));
        }
        let value: Value = response.json().await?;
        if value.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(api_error(&value, "无法建立网盘会话"));
        }
        let mut urls = vec![url];
        for host in [
            "pan.quark.cn",
            "drive.quark.cn",
            "drive-pc.quark.cn",
            "uop.quark.cn",
        ] {
            urls.push(Url::parse(&format!("https://{host}/"))?);
        }
        let headers = urls
            .iter()
            .filter_map(|u| self.jar.cookies(u))
            .filter_map(|h| h.to_str().ok().map(str::to_owned))
            .collect::<Vec<_>>();
        Session::from_cookie_header(&headers.join("; "))
    }
}

fn parse_bootstrap(html: &str) -> Result<(BTreeMap<String, String>, usize)> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("form#u_form input[name]")
        .map_err(|_| Error::Other("无法解析短信登录页".into()))?;
    let mut fields = BTreeMap::new();
    let mut length = 4;
    for input in document.select(&selector) {
        let element = input.value();
        let name = element.attr("name").unwrap_or_default();
        if name == "sms_code" {
            length = element
                .attr("maxlength")
                .and_then(|s| s.parse().ok())
                .filter(|n| (1..=12).contains(n))
                .unwrap_or(4);
        }
        if !name.is_empty() {
            fields.insert(
                name.to_owned(),
                element.attr("value").unwrap_or_default().to_owned(),
            );
        }
    }
    if fields.get("client_id").map(String::as_str) != Some("532")
        || !fields.contains_key("login_name")
        || !fields.contains_key("sms_code")
    {
        return Err(Error::Other(
            "官方短信登录页结构发生变化，无法安全继续".into(),
        ));
    }
    Ok((fields, length))
}

fn status(value: &Value) -> Option<i64> {
    let s = value.get("status").or_else(|| value.get("code"))?;
    s.as_i64()
        .or_else(|| s.as_str().and_then(|v| v.parse().ok()))
}
fn api_error(value: &Value, fallback: &str) -> Error {
    let message = value
        .get("message")
        .or_else(|| value.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or(fallback);
    Error::api(
        200,
        status(value).unwrap_or(0),
        message.chars().take(200).collect::<String>(),
    )
}

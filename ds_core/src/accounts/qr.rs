//! 微信扫码登录（WeChat QR login）—— 免书签、免 F12、免安装
//!
//! ## 为什么需要它
//!
//! 浏览器书签方案要求用户「拖按钮到书签栏」这个一次性动作。微信扫码把它彻底
//! 去掉：面板出二维码 → 微信扫一下 → 服务端拿到 token，零安装、零拖拽、零 F12。
//!
//! ## 链路（4 个 HTTP 请求，全部纯服务端）
//!
//! 1. `GET open.weixin.qq.com/connect/qrconnect?appid=…&redirect_uri=…`
//!    → HTML 里含 `var fordevtool = "<长轮询 URL>"`，从中取 `uuid`；
//! 2. `GET open.weixin.qq.com/connect/qrcode/<uuid>` → PNG 字节（直接下发渲染）；
//! 3. `GET long.open.weixin.qq.com/connect/l/qrconnect?uuid=…` 每 2s 轮询，
//!    解析 `window.wx_errcode` / `window.wx_code`；
//!    - 408 = 未扫码 · 404 = 已扫待确认 · 405 = 已确认（带 wx_code）· 402/403 = 过期；
//! 4. `GET <callback>?code=<wx_code>&state=`，**手动跟随重定向**（不用自动跟随），
//!    从 Location / URL 里抠 `nonce` + `provider`，再
//!    `POST <auth>/users/oauth/get_token {"nonce","provider"}` 换 token。
//!
//! ## 关于 device_id（重要，别再加回来）
//!
//! `device_id` 是 `POST /api/v0/users/login`（**密码登录**）的专属风控字段，必须是
//! 浏览器里数美 SDK（`SMSdk.getDeviceId()`）产出的真实指纹，随机值会被判
//! `RISK_DEVICE_DETECTED`（biz_code=11）。
//!
//! **但微信 OAuth 链路不需要它**：三份独立开源实现（Zephyruston/deepseek-cli、
//! WuXuBaiYang/jtech_plugin、HPLAY/DeepSeekWebAPI）的换 token 请求体里都只有
//! `nonce` + `provider`，全仓 grep `device_id|smsdk` 零命中。
//! 所以本模块**不生成、不发送 device_id**——在没有浏览器的 Linux 服务端上
//! 也拿不到合法指纹，而这恰好不影响扫码链路。
//!
//! ## 已知约束（来自 Zephyruston/deepseek-cli README）
//!
//! 微信必须**已绑定** DeepSeek 账号，否则流程会被重定向到 `/mobile_verification`
//! 手机绑定页，纯 HTTP 无法完成。此时本模块返回 `QrError::NotBound`，由上层提示
//! 用户改用书签或先去官网绑定。

use std::time::{Duration, Instant};

use log::{debug, warn};

use super::client::ClientError;

/// DeepSeek 在微信开放平台的 appid（chat.deepseek.com 面）
pub const WECHAT_APPID: &str = "wx932d4fdaf46d5611";
/// 微信开放平台二维码入口
pub const WECHAT_OPEN_BASE: &str = "https://open.weixin.qq.com";
/// 微信长轮询域名（`lp.` 与 `long.` 实测都可用，这里取 `long.`）
pub const WECHAT_LONG_POLL_BASE: &str = "https://long.open.weixin.qq.com";

/// 取二维码页的超时
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// 单次轮询超时
const POLL_TIMEOUT: Duration = Duration::from_secs(6);
/// 单张二维码的有效期（微信侧约 2 分钟，留余量）
const QR_TTL: Duration = Duration::from_secs(110);

/// 扫码流程错误
#[derive(Debug, thiserror::Error)]
pub enum QrError {
    #[error("网络请求失败: {0}")]
    Http(String),
    #[error("解析失败: {0}")]
    Parse(String),
    #[error("二维码已过期")]
    Expired,
    #[error("上游业务错误: code={code}, msg={msg}")]
    Business { code: i64, msg: String },
    /// 微信未绑定 DeepSeek 账号：被重定向到手机绑定页，纯 HTTP 无解
    #[error("该微信未绑定 DeepSeek 账号，请先在官网绑定后再试")]
    NotBound,
}

impl From<ClientError> for QrError {
    fn from(e: ClientError) -> Self {
        QrError::Http(e.to_string())
    }
}

/// 一次扫码会话的服务端状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrPhase {
    /// 已生成二维码，等待用户扫描
    Waiting,
    /// 已扫描，等待手机端确认
    Scanned,
    /// 已完成，拿到 token
    Done(String),
    /// 已过期（需重新生成）
    Expired,
    /// 失败（含未绑定等）
    Failed(String),
}

/// 一次扫码会话
#[derive(Debug)]
pub struct QrSession {
    uuid: String,
    created_at: Instant,
    /// 上一次轮询拿到的 errcode（微信长轮询要求带 `last`）
    last: u32,
    phase: QrPhase,
}

impl QrSession {
    #[must_use]
    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    #[must_use]
    pub const fn phase(&self) -> &QrPhase {
        &self.phase
    }

    /// 二维码是否已被扫过（前端用来显示「等待确认」）
    #[must_use]
    pub const fn is_scanned(&self) -> bool {
        matches!(self.phase, QrPhase::Scanned)
    }

    /// 距过期还剩多少秒（已过期返回 0）
    #[must_use]
    pub fn ttl_secs(&self) -> u64 {
        let elapsed = self.created_at.elapsed();
        QR_TTL.as_secs().saturating_sub(elapsed.as_secs())
    }

    #[must_use]
    pub const fn is_expired(&self) -> bool {
        self.ttl_secs() == 0
    }
}

/// 微信扫码客户端
///
/// 与 `DsClient` 分开构造：扫码走的是微信/DeepSeek 的 OAuth 面，不是
/// `/api/v0/chat/completion` 面，UA 必须用桌面浏览器（微信授权页只认浏览器 UA）。
#[derive(Debug, Clone)]
pub struct WeChatQrClient {
    http: wreq::Client,
    /// DeepSeek 的 OAuth callback 基址（chat 面）
    callback_base: String,
    /// 换 token 的 endpoint 基址（platform 面）
    auth_base: String,
}

impl WeChatQrClient {
    /// 构造客户端
    ///
    /// `emulation` 决定 TLS 指纹；扫码链路面向微信开放平台，用桌面 Chrome 档位。
    #[must_use]
    pub fn new(emulation: super::client::EmulationProfile) -> Self {
        let http = wreq::Client::builder()
            .emulation(emulation.to_wreq())
            .build()
            .unwrap_or_else(|_| wreq::Client::new());
        Self {
            http,
            callback_base: "https://chat.deepseek.com/api/v0/users/oauth/wechat".to_string(),
            auth_base: "https://chat.deepseek.com/api/v0/users".to_string(),
        }
    }

    /// 覆盖 endpoint 基址（测试与未来上游改版用）
    #[must_use]
    pub fn with_bases(mut self, callback_base: &str, auth_base: &str) -> Self {
        self.callback_base = callback_base.trim_end_matches('/').to_string();
        self.auth_base = auth_base.trim_end_matches('/').to_string();
        self
    }

    fn browser_headers() -> wreq::header::HeaderMap {
        let mut h = wreq::header::HeaderMap::new();
        let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";
        if let Ok(v) = wreq::header::HeaderValue::from_str(ua) {
            h.insert(wreq::header::USER_AGENT, v);
        }
        if let Ok(v) = wreq::header::HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8") {
            h.insert(wreq::header::ACCEPT_LANGUAGE, v);
        }
        if let Ok(v) = wreq::header::HeaderValue::from_static("https://open.weixin.qq.com/") {
            h.insert(wreq::header::REFERER, v);
        }
        h
    }

    /// 第 1 步：开始一次扫码会话（取 uuid）
    pub async fn start(&self) -> Result<QrSession, QrError> {
        let ts = chrono_like_millis();
        let redirect_uri = format!("{}/callback", self.callback_base);
        let url = format!(
            "{}/connect/qrconnect?appid={}&scope=snsapi_login&redirect_uri={}&state=&\
             login_type=jssdk&self_redirect=false&stylelite=1&fast_login=0&ts={}",
            WECHAT_OPEN_BASE,
            WECHAT_APPID,
            urlencode(&redirect_uri),
            ts
        );

        let resp = self
            .http
            .get(&url)
            .timeout(FETCH_TIMEOUT)
            .headers(Self::browser_headers())
            .send()
            .await
            .map_err(|e| QrError::Http(format!("取二维码页失败: {e}")))?;

        let status = resp.status();
        let html = resp
            .text()
            .await
            .map_err(|e| QrError::Http(format!("读取二维码页失败: {e}")))?;
        if !status.is_success() {
            return Err(QrError::Http(format!("取二维码页返回 {status}")));
        }

        // HPLAY 的做法：页面里 `var fordevtool = "https://long.../qrconnect?uuid=XXX"`
        let uuid = extract_fordevtool_uuid(&html).or_else(|| extract_uuid_loose(&html));
        let uuid = uuid.ok_or_else(|| {
            QrError::Parse("二维码页里未找到 fordevtool / uuid（微信页面结构可能已改版）".into())
        })?;

        debug!(target: "ds_core::qr", "微信扫码会话已开始: uuid={}", uuid);
        Ok(QrSession {
            uuid,
            created_at: Instant::now(),
            last: 0,
            phase: QrPhase::Waiting,
        })
    }

    /// 第 2 步：取二维码图片字节（PNG），供面板直接渲染
    pub async fn qr_png(&self, session: &QrSession) -> Result<Vec<u8>, QrError> {
        let url = format!("{}/connect/qrcode/{}", WECHAT_OPEN_BASE, session.uuid);
        let resp = self
            .http
            .get(&url)
            .timeout(FETCH_TIMEOUT)
            .headers(Self::browser_headers())
            .send()
            .await
            .map_err(|e| QrError::Http(format!("取二维码图片失败: {e}")))?;
        if !resp.status().is_success() {
            return Err(QrError::Http(format!("取二维码图片返回 {}", resp.status())));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| QrError::Http(format!("读取二维码图片失败: {e}")))
    }

    /// 第 3 步：轮询一次，推进会话状态
    ///
    /// 返回当前 phase。已确认时会继续走第 4 步换 token。
    pub async fn poll(&self, session: &mut QrSession) -> Result<QrPhase, QrError> {
        if session.is_expired() {
            session.phase = QrPhase::Expired;
            return Ok(QrPhase::Expired);
        }
        if let QrPhase::Done(t) = &session.phase {
            return Ok(QrPhase::Done(t.clone()));
        }

        let url = format!(
            "{}/connect/l/qrconnect?uuid={}&_={}",
            WECHAT_LONG_POLL_BASE,
            session.uuid,
            chrono_like_millis()
        );
        let resp = self
            .http
            .get(&url)
            .timeout(POLL_TIMEOUT)
            .headers(Self::browser_headers())
            .send()
            .await
            .map_err(|e| QrError::Http(format!("轮询失败: {e}")))?;
        let text = resp
            .text()
            .await
            .map_err(|e| QrError::Http(format!("读取轮询响应失败: {e}")))?;

        let errcode = extract_errcode(&text);
        let wx_code = extract_wx_code(&text);

        match errcode {
            // 408 = 未扫码
            408 => {
                session.last = 408;
                session.phase = QrPhase::Waiting;
            }
            // 404 = 已扫待确认
            404 => {
                session.last = 404;
                session.phase = QrPhase::Scanned;
            }
            // 405 = 已确认，带 wx_code
            405 if !wx_code.is_empty() => {
                session.last = 405;
                match self.exchange_token(&wx_code).await {
                    Ok(token) => session.phase = QrPhase::Done(token),
                    Err(e) => session.phase = QrPhase::Failed(e.to_string()),
                }
            }
            // 402/403 = 过期或被取消
            402 | 403 => {
                session.last = errcode;
                session.phase = QrPhase::Expired;
            }
            other => {
                session.last = other;
                session.phase = QrPhase::Waiting;
            }
        }
        Ok(session.phase.clone())
    }

    /// 取账号身份（邮箱 / 手机号）
    ///
    /// 微信 OAuth 的换 token 响应里不带邮箱/手机号，而链接令牌存储需要账号标识，
    /// 因此额外打一次 `/users/current`。失败不阻断（由调用方兜底 key）。
    pub async fn users_current(&self, token: &str) -> Result<(String, String), QrError> {
        let url = format!("{}/current", self.auth_base);
        let mut headers = Self::browser_headers();
        match wreq::header::HeaderValue::from_str(&format!("Bearer {token}")) {
            Ok(v) => {
                headers.insert(wreq::header::AUTHORIZATION, v);
            }
            Err(_) => return Err(QrError::Parse("token 无法作为请求头".into())),
        }

        let resp = self
            .http
            .get(&url)
            .timeout(FETCH_TIMEOUT)
            .headers(headers)
            .send()
            .await
            .map_err(|e| QrError::Http(format!("取账号信息失败: {e}")))?;
        let text = resp
            .text()
            .await
            .map_err(|e| QrError::Http(format!("读取账号信息失败: {e}")))?;

        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| QrError::Parse(format!("账号信息不是 JSON: {e}")))?;

        // 与登录响应同构：data.biz_data.user.{email,mobile_number}
        let user = v
            .pointer("/data/biz_data/user")
            .or_else(|| v.pointer("/data/user"))
            .or_else(|| v.pointer("/data"));
        let get = |k: &str| -> String {
            user.and_then(|u| u.get(k))
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        Ok((get("email"), get("mobile_number")))
    }

    /// 第 4 步：用 wx_code 换 userToken
    ///
    /// 4a. `GET callback?code=&state=` 手动跟随重定向，从 Location/URL 抠
    ///     `nonce` + `provider`；
    /// 4b. `POST /users/oauth/get_token {"nonce","provider"}` → token。
    async fn exchange_token(&self, wx_code: &str) -> Result<String, QrError> {
        let (nonce, provider) = self.fetch_oauth_params(wx_code).await?;
        self.get_token(&nonce, &provider).await
    }

    async fn fetch_oauth_params(&self, wx_code: &str) -> Result<(String, String), QrError> {
        let mut url = format!("{}/callback?code={}&state=", self.callback_base, wx_code);
        let mut location: Option<String> = None;

        // 手动跟随（最多 6 跳），避免自动跟随把 nonce 吞掉
        for _ in 0..6 {
            let resp = self
                .http
                .get(&url)
                .timeout(FETCH_TIMEOUT)
                .headers(Self::browser_headers())
                .send()
                .await
                .map_err(|e| QrError::Http(format!("OAuth callback 失败: {e}")))?;

            if !resp.status().is_redirection() {
                // 终点：直接尝试从最终 URL 或响应体里抠
                let body = resp.text().await.unwrap_or_default();
                if let Some((n, p)) = parse_oauth_params(&body) {
                    return Ok((n, p));
                }
                if let Some((n, p)) = parse_oauth_params(&url) {
                    return Ok((n, p));
                }
                if body.contains("/mobile_verification") || url.contains("/mobile_verification") {
                    return Err(QrError::NotBound);
                }
                return Err(QrError::Parse(
                    "callback 响应里未找到 nonce/provider（上游结构可能已变）".into(),
                ));
            }

            let loc = resp
                .headers()
                .get(wreq::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(std::string::ToString::to_string);
            match loc {
                Some(l) => {
                    if l.contains("/mobile_verification") {
                        return Err(QrError::NotBound);
                    }
                    if let Some((n, p)) = parse_oauth_params(&l) {
                        return Ok((n, p));
                    }
                    url = absolutize(&self.callback_base, &l);
                    location = Some(l);
                }
                None => {
                    return Err(QrError::Parse("重定向响应缺少 Location 头".into()));
                }
            }
        }
        let _ = location;
        Err(QrError::Parse("OAuth callback 重定向次数过多".into()))
    }

    async fn get_token(&self, nonce: &str, provider: &str) -> Result<String, QrError> {
        let url = format!("{}/oauth/get_token", self.auth_base);
        let mut headers = Self::browser_headers();
        if let Ok(v) = wreq::header::HeaderValue::from_static("application/json") {
            headers.insert(wreq::header::CONTENT_TYPE, v);
        }
        if let Ok(v) = wreq::header::HeaderValue::from_static("application/json") {
            headers.insert(wreq::header::ACCEPT, v);
        }
        if let Ok(v) = wreq::header::HeaderValue::from_static("https://chat.deepseek.com") {
            headers.insert(wreq::header::ORIGIN, v);
        }

        let body = serde_json::json!({ "nonce": nonce, "provider": provider });
        let resp = self
            .http
            .post(&url)
            .timeout(FETCH_TIMEOUT)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| QrError::Http(format!("换 token 失败: {e}")))?;

        let text = resp
            .text()
            .await
            .map_err(|e| QrError::Http(format!("读取换 token 响应失败: {e}")))?;

        // Zephyruston/deepseek-cli 的解析顺序：/data/biz_data/token → /data/token
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| QrError::Parse(format!("换 token 响应不是 JSON: {e}")))?;

        if let Some(code) = v.pointer("/data/biz_code").and_then(|c| c.as_i64()) {
            if code != 0 {
                let msg = v
                    .pointer("/data/biz_msg")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string();
                return Err(QrError::Business { code, msg });
            }
        }

        let token = v
            .pointer("/data/biz_data/token")
            .or_else(|| v.pointer("/data/token"))
            .or_else(|| v.pointer("/data/biz_data/user/token"))
            .and_then(|t| t.as_str())
            .map(std::string::ToString::to_string)
            .filter(|t| !t.trim().is_empty());

        match token {
            Some(t) => {
                debug!(target: "ds_core::qr", "微信扫码换 token 成功（长度 {}）", t.len());
                Ok(t)
            }
            None => Err(QrError::Parse(format!(
                "换 token 响应里没有 token 字段: {}",
                truncate(&text, 200)
            ))),
        }
    }
}

// ── 纯函数（可单测）────────────────────────────────────────────────────────

/// 从微信授权页 HTML 提取 `var fordevtool = "..."` 里的 uuid
fn extract_fordevtool_uuid(html: &str) -> Option<String> {
    let re = html.find("fordevtool")?;
    let rest = &html[re..];
    let start = rest.find('"')?;
    let after = &rest[start + 1..];
    let end = after.find('"')?;
    let url = &after[..end];
    extract_uuid_loose(url)
}

/// 从任意字符串里抠 `uuid=<值>`（值取 `[A-Za-z0-9_-]`）
fn extract_uuid_loose(s: &str) -> Option<String> {
    let idx = s.find("uuid=")?;
    let rest = &s[idx + 5..];
    let val: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if val.is_empty() { None } else { Some(val) }
}

/// 解析轮询响应里的 `window.wx_errcode=NNN`
fn extract_errcode(s: &str) -> u32 {
    s.find("wx_errcode")
        .and_then(|i| {
            let rest = &s[i..];
            let eq = rest.find('=')?;
            let after = &rest[eq + 1..];
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u32>().ok()
        })
        .unwrap_or(0)
}

/// 解析轮询响应里的 `window.wx_code='XXX'`
fn extract_wx_code(s: &str) -> String {
    s.find("wx_code")
        .and_then(|i| {
            let rest = &s[i..];
            let eq = rest.find('=')?;
            let after = &rest[eq + 1..];
            let q = after.find(['\'', '"'])?;
            let inner = &after[q + 1..];
            let end = inner.find(['\'', '"'])?;
            Some(inner[..end].to_string())
        })
        .unwrap_or_default()
}

/// 从 Location / URL / 响应体里抠 `nonce` 与 `provider`
///
/// 两种形态都要支持：重定向 Location 是查询串（`?nonce=…&provider=…`），
/// 而 callback 直接返回时是 JSON（`{"data":{"nonce":"…","provider":"…"}}`）。
/// 只写其中一种会在另一形态下解析失败（单测已覆盖）。
pub(crate) fn parse_oauth_params(s: &str) -> Option<(String, String)> {
    let nonce = extract_query_value(s, "nonce").or_else(|| extract_json_value(s, "nonce"))?;
    let provider = extract_query_value(s, "provider")
        .or_else(|| extract_json_value(s, "provider"))
        .unwrap_or_else(|| "WECHAT".to_string());
    Some((nonce, provider))
}

fn extract_query_value(s: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=");
    let idx = s.find(&pat)?;
    let rest = &s[idx + pat.len()..];
    let val: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    if val.is_empty() { None } else { Some(val) }
}

fn extract_json_value(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let idx = s.find(&pat)?;
    let rest = &s[idx + pat.len()..];
    let after_colon = &rest[rest.find(':')? + 1..];
    let q = after_colon.find('"')?;
    let inner = &after_colon[q + 1..];
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

/// 把可能相对的重定向地址补全为绝对 URL
fn absolutize(base: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    // callback base 形如 https://chat.deepseek.com/api/v0/users/oauth/wechat
    // 取到 origin 后拼接
    if let Some(end) = base.find("://") {
        let rest = &base[end + 3..];
        let host_end = rest.find('/').unwrap_or(rest.len());
        let origin = &base[..end + 3 + host_end];
        if let Some(p) = loc.strip_prefix('/') {
            return format!("{origin}/{p}");
        }
        return format!("{origin}/{loc}");
    }
    loc.to_string()
}

/// 最小 URL 编码（只处理本模块会用到的字符）
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

/// 毫秒时间戳（不引入 chrono：ds_core 侧无该依赖，用 SystemTime 即可）
fn chrono_like_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_uuid_from_fordevtool() {
        let html = r#"<script>var fordevtool = "https://long.open.weixin.qq.com/connect/l/qrconnect?uuid=0513tRvF1L1Skl27";</script>"#;
        assert_eq!(
            extract_fordevtool_uuid(html),
            Some("0513tRvF1L1Skl27".to_string())
        );
    }

    #[test]
    fn extracts_uuid_loose_from_plain_url() {
        assert_eq!(
            extract_uuid_loose("https://x/connect/l/qrconnect?uuid=abc-123_X"),
            Some("abc-123_X".to_string())
        );
    }

    #[test]
    fn parses_poll_response_waiting() {
        let body = "window.wx_errcode=408;window.wx_code='';";
        assert_eq!(extract_errcode(body), 408);
        assert_eq!(extract_wx_code(body), "");
    }

    #[test]
    fn parses_poll_response_confirmed() {
        let body = "window.wx_errcode=405;window.wx_code='031abcDEF';";
        assert_eq!(extract_errcode(body), 405);
        assert_eq!(extract_wx_code(body), "031abcDEF");
    }

    #[test]
    fn parses_poll_response_scanned() {
        let body = "window.wx_errcode=404;window.wx_code='';";
        assert_eq!(extract_errcode(body), 404);
    }

    #[test]
    fn parses_expired_codes() {
        for code in ["402", "403"] {
            let body = format!("window.wx_errcode={code};window.wx_code='';");
            assert_eq!(extract_errcode(&body), code.parse::<u32>().unwrap());
        }
    }

    #[test]
    fn extracts_oauth_params_from_location() {
        let loc = "https://chat.deepseek.com/oauth?nonce=abc123XYZ&provider=WECHAT";
        let (n, p) = parse_oauth_params(loc).expect("should parse");
        assert_eq!(n, "abc123XYZ");
        assert_eq!(p, "WECHAT");
    }

    #[test]
    fn extracts_oauth_params_from_json_body() {
        let body = r#"{"data":{"nonce":"n1","provider":"WECHAT"}}"#;
        let (n, p) = parse_oauth_params(body).expect("should parse");
        assert_eq!(n, "n1");
        assert_eq!(p, "WECHAT");
    }

    #[test]
    fn urlencode_escapes_colon_and_slash() {
        let enc = urlencode("https://chat.deepseek.com/api/v0/users/oauth/wechat/callback");
        assert!(enc.contains("%3A%2F%2F"));
        assert!(!enc.contains(':'));
    }

    #[test]
    fn absolutizes_relative_location() {
        let base = "https://chat.deepseek.com/api/v0/users/oauth/wechat";
        assert_eq!(
            absolutize(base, "/mobile_verification?x=1"),
            "https://chat.deepseek.com/mobile_verification?x=1"
        );
        assert_eq!(
            absolutize(base, "https://other.example/x"),
            "https://other.example/x"
        );
    }

    #[test]
    fn session_ttl_counts_down() {
        let s = QrSession {
            uuid: "u".to_string(),
            created_at: Instant::now(),
            last: 0,
            phase: QrPhase::Waiting,
        };
        assert!(s.ttl_secs() > 100);
        assert!(!s.is_expired());
    }

    #[test]
    fn truncate_short_and_long() {
        assert_eq!(truncate("abc", 10), "abc");
        assert!(truncate(&"x".repeat(300), 200).ends_with('…'));
    }
}

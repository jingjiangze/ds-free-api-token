//! 微信扫码登录的管理端路由 —— 免书签、免 F12 的账号接入
//!
//! 三个端点（全部挂在 admin_routes 下，共享 JWT 鉴权）：
//! - `POST /admin/api/accounts/qr/start`  开始一次扫码会话，返回二维码 PNG（base64）+ 会话 id；
//! - `GET  /admin/api/accounts/qr/poll?id=` 轮询一次，推进状态机；
//! - `POST /admin/api/accounts/qr/cancel?id=` 取消并清理。
//!
//! 状态机在进程内持有（DashMap + TTL），不落盘、不记 token 到日志。
//! 扫码成功后**复用已有的 link 路径**写入 `linked-tokens.json` 并热加账号，
//! 与浏览器书签抓取是同一条落库通道，不新增第二套账号存储。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use super::admin::{error_response, json_response};
use super::handlers::AppState;

/// 扫码会话在内存中的最长存活时间（超过即作废）
const SESSION_MAX_AGE: Duration = Duration::from_secs(300);
/// 同时存在的扫码会话上限（防滥用 / 防内存膨胀）
const MAX_SESSIONS: usize = 32;
/// 单会话轮询上限（防止前端失控轮询）
const MAX_POLLS: u32 = 300;

/// 进程内扫码会话表
#[derive(Debug, Default)]
pub struct QrRegistry {
    sessions: DashMap<String, QrEntry>,
}

#[derive(Debug)]
struct QrEntry {
    session: ds_core::QrSession,
    created_at: Instant,
    polls: u32,
    /// 已成功链接过则不再重复落库
    consumed: bool,
}

impl QrRegistry {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 清理过期会话（每次 start 时顺带做，避免常驻任务）
    fn sweep(&self) {
        self.sessions
            .retain(|_, e| e.created_at.elapsed() < SESSION_MAX_AGE);
    }
}

/// 前端可见的扫码状态
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QrStatusView {
    /// 等待扫码
    Waiting,
    /// 已扫描，等待手机端确认
    Scanned,
    /// 成功（账号已写入）
    Done,
    /// 二维码过期，需要重新生成
    Expired,
    /// 失败，`message` 给出原因
    Failed,
}

#[derive(Debug, Serialize)]
pub struct QrStartResponse {
    pub id: String,
    /// 二维码 PNG 的 base64（`data:image/png;base64,` 前缀由前端拼）
    pub png_base64: String,
    /// 剩余有效秒数
    pub ttl: u64,
}

#[derive(Debug, Serialize)]
pub struct QrPollResponse {
    pub status: QrStatusView,
    /// 剩余有效秒数（Done/Failed 时为 0）
    pub ttl: u64,
    /// 失败原因（i18n key 或服务端原文）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 成功时回显的账号标识（脱敏）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QrPollQuery {
    pub id: String,
}

/// POST /admin/api/accounts/qr/start
pub(crate) async fn admin_qr_start(State(state): State<AppState>) -> Response {
    let registry = match &state.qr_registry {
        Some(r) => r,
        None => return error_response(StatusCode::NOT_FOUND, "扫码登录未启用"),
    };
    registry.sweep();
    if registry.sessions.len() >= MAX_SESSIONS {
        return error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "同时进行的扫码会话过多，请稍后重试",
        );
    }

    let client = ds_core::WeChatQrClient::new(ds_core::EmulationProfile::Chrome136);
    let mut session = match client.start().await {
        Ok(s) => s,
        Err(e) => {
            log::warn!(target: "server::admin", "微信扫码会话创建失败: {e}");
            return error_response(StatusCode::BAD_GATEWAY, &format!("无法创建扫码会话：{e}"));
        }
    };
    let png = match client.qr_png(&session).await {
        Ok(p) => p,
        Err(e) => {
            log::warn!(target: "server::admin", "微信二维码图片获取失败: {e}");
            return error_response(StatusCode::BAD_GATEWAY, &format!("无法获取二维码：{e}"));
        }
    };

    let id = new_session_id();
    let ttl = session.ttl_secs();
    registry.sessions.insert(
        id.clone(),
        QrEntry {
            session,
            created_at: Instant::now(),
            polls: 0,
            consumed: false,
        },
    );
    json_response(&QrStartResponse {
        id,
        png_base64: B64.encode(&png),
        ttl,
    })
}

/// GET /admin/api/accounts/qr/poll?id=...
pub(crate) async fn admin_qr_poll(
    State(state): State<AppState>,
    Query(q): Query<QrPollQuery>,
) -> Response {
    let registry = match &state.qr_registry {
        Some(r) => r,
        None => return error_response(StatusCode::NOT_FOUND, "扫码登录未启用"),
    };

    let mut entry = match registry.sessions.get_mut(&q.id) {
        Some(e) => e,
        None => {
            return json_response(&QrPollResponse {
                status: QrStatusView::Failed,
                ttl: 0,
                message: Some("会话不存在或已结束".to_string()),
                account: None,
            });
        }
    };

    if entry.created_at.elapsed() >= SESSION_MAX_AGE {
        drop(entry);
        registry.sessions.remove(&q.id);
        return json_response(&QrPollResponse {
            status: QrStatusView::Expired,
            ttl: 0,
            message: None,
            account: None,
        });
    }

    entry.polls += 1;
    if entry.polls > MAX_POLLS {
        let msg = Some("轮询次数过多".to_string());
        drop(entry);
        registry.sessions.remove(&q.id);
        return json_response(&QrPollResponse {
            status: QrStatusView::Expired,
            ttl: 0,
            message: msg,
            account: None,
        });
    }

    let client = ds_core::WeChatQrClient::new(ds_core::EmulationProfile::Chrome136);
    let phase = match client.poll(&mut entry.session).await {
        Ok(p) => p,
        Err(e) => {
            log::warn!(target: "server::admin", "微信扫码轮询失败: {e}");
            ds_core::QrPhase::Failed(e.to_string())
        }
    };

    let ttl = entry.session.ttl_secs();

    match phase {
        ds_core::QrPhase::Waiting => json_response(&QrPollResponse {
            status: QrStatusView::Waiting,
            ttl,
            message: None,
            account: None,
        }),
        ds_core::QrPhase::Scanned => json_response(&QrPollResponse {
            status: QrStatusView::Scanned,
            ttl,
            message: None,
            account: None,
        }),
        ds_core::QrPhase::Expired => {
            let id = q.id.clone();
            drop(entry);
            registry.sessions.remove(&id);
            json_response(&QrPollResponse {
                status: QrStatusView::Expired,
                ttl: 0,
                message: None,
                account: None,
            })
        }
        ds_core::QrPhase::Failed(msg) => {
            let id = q.id.clone();
            drop(entry);
            registry.sessions.remove(&id);
            json_response(&QrPollResponse {
                status: QrStatusView::Failed,
                ttl: 0,
                message: Some(msg),
                account: None,
            })
        }
        ds_core::QrPhase::Done(token) => {
            let already = entry.consumed;
            entry.consumed = true;
            let id = q.id.clone();
            // 先释放会话表锁，再落库（落库会写配置锁并热重载账号池，
            // 持有 DashMap 分片锁跨这些 await 没有收益且易埋死锁隐患）
            drop(entry);
            if already {
                return json_response(&QrPollResponse {
                    status: QrStatusView::Done,
                    ttl: 0,
                    message: None,
                    account: None,
                });
            }
            match link_token_into_pool(&state, &token).await {
                Ok(display) => {
                    registry.sessions.remove(&id);
                    json_response(&QrPollResponse {
                        status: QrStatusView::Done,
                        ttl: 0,
                        message: None,
                        account: Some(display),
                    })
                }
                Err(e) => {
                    registry.sessions.remove(&id);
                    json_response(&QrPollResponse {
                        status: QrStatusView::Failed,
                        ttl: 0,
                        message: Some(e),
                        account: None,
                    })
                }
            }
        }
    }
}

/// POST /admin/api/accounts/qr/cancel?id=...
pub(crate) async fn admin_qr_cancel(
    State(state): State<AppState>,
    Query(q): Query<QrPollQuery>,
) -> Response {
    if let Some(r) = &state.qr_registry {
        r.sessions.remove(&q.id);
    }
    json_response(&serde_json::json!({ "ok": true }))
}

/// 把扫码拿到的 token 写入链接令牌存储 + 热加账号（复用 link 路径）
///
/// 与 `admin_link_account` 的区别：扫码链路拿不到 email/mobile（微信不返回），
/// 因此先调 `/users/current` 取身份；取不到则以 `wechat:<token 前缀>` 作为
/// 兜底 key，保证账号仍能进池（面板里可随后补邮箱）。
async fn link_token_into_pool(state: &AppState, token: &str) -> Result<String, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("扫码结果为空".to_string());
    }

    // 取身份：与 pool 里的 DsClient 同源，这里直接用扫码 client 打 users/current
    let (email, mobile) = fetch_identity(token).await.unwrap_or_default();

    let key = if !email.is_empty() {
        email.clone()
    } else if !mobile.is_empty() {
        mobile.clone()
    } else {
        // 兜底：微信账号没有可见邮箱/手机号时，用 token 前缀当 key
        format!("wechat:{}", &token.chars().take(8).collect::<String>())
    };

    if let Err(e) = ds_core::store_linked_token(&key, token, "", "") {
        return Err(format!("写入链接令牌失败: {e}"));
    }

    // 合并账号进配置：密码留空（命中链接令牌会跳过密码登录）
    let mut new_config = { state.config.read().await.clone() };
    let matched = new_config.ds_core.accounts.iter().find(|a| {
        let k = if a.email.is_empty() {
            a.mobile.as_str()
        } else {
            a.email.as_str()
        };
        k == key
    });
    if matched.is_none() {
        new_config.ds_core.accounts.push(crate::config::Account {
            email: email.clone(),
            mobile: mobile.clone(),
            area_code: String::new(),
            password: String::new(),
            device_id: String::new(),
        });
    }
    if let Err(e) = new_config.validate() {
        return Err(format!("配置校验失败: {e}"));
    }

    // 落盘（与 admin_link_account 同构：先写内存再 save）
    {
        let mut guard = state.config.write().await;
        *guard = new_config.clone();
        if let Err(e) = guard.save(&state.config_path) {
            return Err(format!("配置保存失败: {e}"));
        }
    }

    // 热重载账号池（命中链接令牌 → 跳过密码登录）
    state
        .adapter
        .sync_accounts(&new_config.ds_core.accounts)
        .await;

    let pool_state = state
        .adapter
        .account_statuses()
        .into_iter()
        .find(|a| {
            let k = if a.email.is_empty() {
                a.mobile.as_str()
            } else {
                a.email.as_str()
            };
            k == key.as_str()
        })
        .map(|a| a.state);

    log::info!(
        target: "http::server",
        "微信扫码链接已提交: {key}（池状态: {:?}，不记录 token）",
        pool_state
    );

    match pool_state {
        Some(s) => Ok(mask(&key)),
        None => {
            Err("账号已保存但未通过验证：token 可能已失效，请重新扫码或改用书签方式".to_string())
        }
    }
}

/// 调 `/users/current` 取邮箱/手机号；失败返回 None（不阻断）
async fn fetch_identity(token: &str) -> Option<(String, String)> {
    let client = ds_core::WeChatQrClient::new(ds_core::EmulationProfile::Chrome136);
    client.users_current(token).await.ok()
}

/// 脱敏显示（邮箱保留首字符与域名，手机号保留后 4 位）
fn mask(key: &str) -> String {
    if let Some(at) = key.find('@') {
        let (name, domain) = key.split_at(at);
        let first: String = name.chars().take(1).collect();
        return format!("{first}***{domain}");
    }
    let len = key.chars().count();
    if len <= 4 {
        return "***".to_string();
    }
    let tail: String = key.chars().skip(len - 4).collect();
    format!("***{tail}")
}

fn new_session_id() -> String {
    use rand::RngExt;
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

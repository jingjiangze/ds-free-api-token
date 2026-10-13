//! 用会话令牌取账号身份（邮箱 / 手机号）
//!
//! 为什么单独放一个模块：浏览器抓取脚本拿到令牌后，本来会顺带用该令牌调一次
//! DeepSeek 取账号信息。但抓取发生在用户的浏览器里，任何一个环节失败（令牌
//! 带壳、请求被拦、页面状态不对）都会导致邮箱/手机号缺失，而账号入库又必须
//! 有一个标识。这里由**服务端**用令牌补取一次身份，把「抓取只拿到令牌」也
//! 变成可用路径，同时顺带验证令牌是否真的有效（无效令牌会立刻拿到非 0 业务码）。

use std::time::Duration;

use super::client::EmulationProfile;

/// 单次取身份的超时
const TIMEOUT: Duration = Duration::from_secs(10);

/// 账号身份（两者都可能为空，由调用方决定如何兜底）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserIdentity {
    pub email: String,
    pub mobile: String,
}

/// 用令牌调 `/users/current` 取身份
///
/// 返回 `Err` 表示令牌不可用（网络失败 / 业务码非 0 / 结构不符），调用方应
/// 据此提示用户令牌可能已失效。
pub async fn fetch_user_identity(token: &str) -> Result<UserIdentity, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("令牌为空".to_string());
    }

    let http = wreq::Client::builder()
        .emulation(EmulationProfile::Chrome136.to_wreq())
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;

    let mut headers = wreq::header::HeaderMap::new();
    headers.insert(
        wreq::header::USER_AGENT,
        wreq::header::HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36",
        ),
    );
    headers.insert(
        wreq::header::ACCEPT,
        wreq::header::HeaderValue::from_static("application/json"),
    );
    let auth = wreq::header::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "令牌含非法字符，无法作为请求头发送".to_string())?;
    headers.insert(wreq::header::AUTHORIZATION, auth);

    let resp = http
        .get("https://chat.deepseek.com/api/v0/users/current")
        .timeout(TIMEOUT)
        .headers(headers)
        .send()
        .await
        .map_err(|e| format!("请求 /users/current 失败: {e}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取 /users/current 响应失败: {e}"))?;
    if !status.is_success() {
        return Err(format!("取账号信息返回 {status}"));
    }

    parse_identity(&text)
}

/// 解析 `/users/current` 响应
///
/// 真实形态：`{"code":0,"data":{"biz_code":0,"biz_data":{"user":{...}}}}`；
/// 兼容 `data.user` / `data` 直接就是 user 的变体。
pub(crate) fn parse_identity(body: &str) -> Result<UserIdentity, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("账号信息不是 JSON: {e}"))?;

    // 业务码非 0 直接判为令牌无效
    if let Some(code) = v
        .pointer("/data/biz_code")
        .and_then(serde_json::Value::as_i64)
    {
        if code != 0 {
            let msg = v
                .pointer("/data/biz_msg")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            return Err(format!("取账号信息业务错误: code={code}, msg={msg}"));
        }
    }

    let user = v
        .pointer("/data/biz_data/user")
        .or_else(|| v.pointer("/data/user"))
        .or_else(|| v.pointer("/data/biz_data"))
        .or_else(|| v.pointer("/data"))
        .ok_or_else(|| "账号信息结构不符（响应里没有 data）".to_string())?;

    let get = |k: &str| -> String {
        user.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    Ok(UserIdentity {
        email: get("email"),
        mobile: get("mobile_number"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_shape() {
        let body = r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":{"user":{"id":"u1","email":"a@b.com","mobile_number":"13800138000"}}}}"#;
        let id = parse_identity(body).expect("should parse");
        assert_eq!(id.email, "a@b.com");
        assert_eq!(id.mobile, "13800138000");
    }

    #[test]
    fn parses_flat_variant() {
        let body = r#"{"data":{"email":"x@y.z","mobile_number":"13900139000"}}"#;
        let id = parse_identity(body).expect("should parse");
        assert_eq!(id.email, "x@y.z");
        assert_eq!(id.mobile, "13900139000");
    }

    #[test]
    fn non_zero_biz_code_is_error() {
        let body = r#"{"data":{"biz_code":40003,"biz_msg":"TOKEN_EXPIRED"}}"#;
        let e = parse_identity(body).unwrap_err();
        assert!(e.contains("40003"), "{e}");
        assert!(e.contains("TOKEN_EXPIRED"), "{e}");
    }

    #[test]
    fn recognized_but_empty_yields_empty_identity() {
        // 信封认得出来、只是没有账号字段 → 返回空身份，由调用方决定怎么兜底
        // （比判成「令牌无效」更准确：令牌可能是好的，只是没这个字段）
        let body = r#"{"data":{"biz_code":0,"biz_data":{}}}"#;
        let id = parse_identity(body).expect("envelope recognized");
        assert_eq!(id, UserIdentity::default());
    }

    #[test]
    fn missing_data_is_error() {
        assert!(parse_identity(r#"{"code":0}"#).is_err());
    }

    #[test]
    fn non_json_is_error() {
        assert!(parse_identity("<html>challenge</html>").is_err());
    }
}

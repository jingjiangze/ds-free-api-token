# sub2api 参考逻辑审计 + 「跳转自动抓取」改造方案

日期：2026-10-12　范围：`_tmp/ds-free-api-token-src`（ds-free-api v0.5.4 fork）
输入需求：用户「这个脚本用不了，改成跳转过去后自动获取，然后在浏览器域名这里直接复制（可以参考 sub2api 的逻辑）」

## 1. sub2api 实际逻辑（已核实，非推测）

仓库 `Wei-Shaw/sub2api`（Go 后端 + Vue 前端），浏览器登录链路核实如下：

| 机制 | sub2api 的做法 | 出处 |
|---|---|---|
| Cookie / sessionKey 接入 | **面板上一个大文本框**，用户自己从浏览器 DevTools 复制 sessionKey 粘进去，点「开始自动授权」；配套一个「如何获取 sessionKey」的折叠说明（登录 → F12 → Application/Storage → 找 sessionKey 行） | `frontend/src/components/account/OAuthAuthorizationFlow.vue` L530-640；`frontend/src/i18n/locales/en/admin/accounts.ts` L1144-1160（`cookieAutoAuth` / `howToGetSessionKey` / `step1..6`） |
| OAuth 授权码 | 真 OAuth 平台（OpenAI/Gemini/Grok/Antigravity）才走「生成授权 URL → 用户授权 → 粘回 code」；前端 watch 输入框，粘进来的 URL 自动解析出 `code`/`state` | 同上文件 L1140-1170「Auto-extract code from callback URL」 |
| DeepSeek | sub2api **没有** DeepSeek 接入（树内无 deepseek 相关文件） | `git trees/main?recursive=1` 全量枚举，0 命中 |

结论：**sub2api 的「自动」= 后端拿用户粘来的 sessionKey 自己去换授权，不是浏览器侧自动抓取**。用户要的「跳转过去后自动获取」比 sub2api 更进一步——抓取这一步也要免 F12。

## 2. 现有 v0.5.4 为什么「用不了」

当前链路：面板给一段 **F12 控制台脚本**（`ConfigPage.tsx` 的 `LINK_SNIPPET`），要用户在 chat.deepseek.com 页面打开 DevTools、粘贴执行，再把控制台 JSON 粘回来。

三个断点（均为实测过的）：
1. **DeepSeek 的 `userToken` 在 `localStorage`**（已核实：`main.19cce7bd22.js` 里 `"userToken"===e.key?"token":…`、`localStorage.getItem(i)`），控制台能读，但普通用户不会开 DevTools；
2. **`SMSdk.getDeviceId()` 只在 DeepSeek 页面上下文里存在**（数美设备指纹 SDK，`window.SMSdk`），在别的域读不到——所以脚本必须贴在 DeepSeek 域名下执行；
3. 跨域 → 面板域（`dsapi.jiangjiangze.icu`）读不到 DeepSeek 的 localStorage / `window.SMSdk`（同源策略）。

所以「免 F12 + 自动抓取」的可行形态只有一个：**让浏览器真的位于 DeepSeek 页面上下文里，然后在那个上下文里把结果送回面板域**。

## 3. 方案：本地抓取页 + postMessage 回传（本次采用）

### 3.1 形态

```
面板（/admin，本域）
  │ 用户点「浏览器登录」→「打开 DeepSeek 登录页」
  ▼
https://chat.deepseek.com/sign_in   ← 用户正常登录（新标签）
  │ 登录完成后，用户点「继续 / 已完成登录」按钮（本方案注入）
  ▼
本域 /ds-capture?t=<一次性票据>    ← 由 DeepSeek 页面 location 跳转过来
  │  同源读不到 DeepSeek 的 token（跨域）⇒ 抓取页用「回跳 DeepSeek 同源页」两段式
```

**两段式抓取（关键设计）**：
- 第一段 `/ds-capture` 由 DeepSeek 页面跳转而来，携带 DeepSeek 域的 `document.referrer` 与 `window.name` 兜底数据；
- 由于 localStorage 跨域不可读，**抓取数据实际由 DeepSeek 页面内的脚本在跳转前通过 `postMessage` 写入 `window.name` / URL hash**——见 3.2。

### 3.2 免 F12 的抓取实现：书签脚本（bookmarklet）

DeepSeek 页面内必须有代码执行才能读 `localStorage.userToken` 与 `window.SMSdk`。不给 F12，唯一通用形态是**书签脚本**：用户把面板上的一个链接拖到书签栏（一次），登录 DeepSeek 后**点一下书签**，脚本自动读三项并跳回本域。

> 这就是「跳转过去后自动获取 + 在浏览器域名这里直接复制」的可落地形态：
> - 「跳转过去」= 面板给出 DeepSeek 登录页直达链接；
> - 「自动获取」= 书签脚本在该域自动读 token / device_id / UA；
> - 「在浏览器域名这里直接复制」= 面板域名下抓取页把结果渲染成一段 JSON，用户点「复制」→ 回面板粘贴（或直接点「提交」，因为抓取页就在本域，可直连管理 API）。

### 3.3 为什么不做「服务端反向代理整个 chat.deepseek.com」

- 上游有 WAF / PoW / 数美风控（`ds_core/src/accounts/pow.rs`、`hif.rs` 就是为对齐它而写），代理整站会破坏指纹并触发风控；
- 上游登录走 `https://chat.deepseek.com/api/v0/users/login`（已从 bundle 核实 `/api/v0/users/...` 路径族），服务端代登录会同时拿到密码，违背「不要账号密码登录」的需求；
- sub2api 对真 OAuth 平台也是「粘 code」而非代理登录页——同一取舍。

### 3.4 安全边界

- 抓取页 `/ds-capture` 只渲染/转发用户自己的浏览器会话数据，**不落盘、不写日志**；
- 提交仍走既有 `POST /admin/api/accounts/link`（JWT 鉴权），token 由面板持有；
- 书签脚本只读 `localStorage.userToken`、`window.SMSdk.getDeviceId()`、`navigator.userAgent`，不发送到任何第三方域。

## 4. 实施清单

| # | 改动 | 文件 |
|---|---|---|
| 1 | 新增 `/ds-capture` GET 路由（public，无 JWT）：渲染抓取页 HTML；带 `t=` 票据时回显该票据 | `src/server.rs` + `src/server/admin.rs` |
| 2 | 抓取页：从 `window.name` / URL hash 解析 DeepSeek 脚本写入的 JSON，渲染 token 摘要 + 「复制 JSON」+ 「提交到本网关」按钮 | 新增静态页（嵌入 WebAssets） |
| 3 | 面板：把「F12 提取脚本」换成「① 打开登录页（新标签）→ ② 点书签（拖我上书签栏）→ ③ 自动跳回本域抓取页 → 复制/提交」 | `web/src/pages/ConfigPage.tsx` |
| 4 | 书签脚本改为 `javascript:` URI 形式，抓取后 `location.href = <面板域>/ds-capture#<json>` | 同上 |
| 5 | i18n 三语言包同步 | `web/src/locales/{zh,en,id}/common.json` |
| 6 | 版本 0.5.4 → 0.5.5，CHANGELOG | `Cargo.toml` / `ds_core/Cargo.toml` / `web/package.json` / `Cargo.lock` |

## 5. 验收标准

1. `/ds-capture` 无 JWT 可访问，返回 HTML（含本域提交按钮）；
2. 带 `#` 数据时可正确解析并展示 token 前 8 位 + device_id + UA；
3. 面板文案不再出现「F12 / 控制台 / 粘贴脚本执行」字样（改为书签一次点击）；
4. 既有 `POST /admin/api/accounts/link` 契约不变（401/200 行为不回退）；
5. GitHub Actions 双平台构建绿，release 产物 SHA256 与 release 页一致；
6. AWS 部署后 health / key / login / 新页面均 PASS。

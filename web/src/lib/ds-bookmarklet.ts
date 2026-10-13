/**
 * 浏览器登录「抓取书签」（bookmarklet）生成器
 *
 * 为什么需要它：`chat.deepseek.com` 的 `localStorage.userToken` 与
 * `window.SMSdk`（数美设备指纹）只在 DeepSeek 页面上下文里可读，跨域
 * 读不到（同源策略）。要在「不打开 F12 控制台」的前提下抓取，唯一通用
 * 形态就是让用户在 DeepSeek 页面上点一次书签。
 *
 * 生成规则：
 * - `javascript:` URI，可直接拖到书签栏或右键复制链接；
 * - 抓取 `localStorage.userToken` + `SMSdk.getDeviceId()` + `navigator.userAgent`
 *   + `/api/v0/users/settings` 返回的账号信息；
 * - 完成后编码进 hash 跳回 `returnUrl`（本域抓取页）。
 *
 * 抓取页与面板同域同源，localStorage 共享：面板登录态（键 `ds-admin-token`，
 * 见 api.ts 的 TOKEN_KEY）在抓取页可读，因此抓取页能「自动授权」——跳回去
 * 即提交，无需再点按钮。改跳转目标时别改 hash 参数名（`d`），抓取页与面板
 * 兜底输入框都按 `#d=` 解析。
 *
 * 注意：源码里不出现字面量反引号（用 `String.fromCharCode(96)` 拼），
 * 避免模板字面量字面量在工具链里被截断。
 */

const BT = String.fromCharCode(96); // `

/**
 * 解包 DeepSeek 网页端的 `userToken`
 *
 * 网页端在 localStorage 里存的是 `{"value":"<token>","__version":"0"}`，
 * 不是裸令牌。整串当 `Authorization: Bearer` 发出去一定被拒，而且用它取账号
 * 信息也会失败（拿不到邮箱）。抓取脚本与服务端两侧都要解包。
 */
export function unwrapUserToken(raw: string): string {
  const t = (raw || '').trim();
  if (t.startsWith('{')) {
    try {
      const o = JSON.parse(t) as { value?: unknown };
      if (o && typeof o.value === 'string' && o.value.trim()) return o.value.trim();
    } catch {
      /* 不是 JSON 就按裸令牌处理 */
    }
  }
  return t;
}

/** 生成抓取书签的 href（`javascript:...`） */
export function buildCaptureBookmarklet(returnUrl: string): string {
  const lines = [
    '(async function(){',
    '  var t = localStorage.getItem("userToken") || "";',
    '  t = String(t).trim();',
    '  if (t.charAt(0) === "{") {',
    '    try { var o = JSON.parse(t); if (o && typeof o.value === "string") t = o.value.trim(); } catch(e){}',
    '  }',
    '  if(!t){ alert("未找到会话令牌：请先在 chat.deepseek.com 完成登录"); return; }',
    '  var d = "";',
    '  var sm = window.SMSdk;',
    '  if (sm && sm.ready) {',
    '    try { await new Promise(function(res){ sm.ready(function(){ res(); }); }); } catch(e){}',
    '  }',
    '  if (sm && sm.getDeviceId) {',
    '    for (var i = 0; i < 60 && !d; i++) {',
    '      try { d = sm.getDeviceId() || ""; } catch(e){}',
    '      if (!d) await new Promise(function(r){ setTimeout(r, 250); });',
    '    }',
    '  }',
    '  var u = {};',
    '  try{',
    '    var r = await fetch("/api/v0/users/current", { headers: { authorization: "Bearer " + t } });',
    '    var j = await r.json();',
    '    u = (j && j.data && j.data.biz_data && j.data.biz_data.user) || (j && j.data && j.data.user) || (j && j.data) || {};',
    '  }catch(e){}',
    '  var email = u.email || "";',
    '  var mobile = u.mobile_number || u.mobile || "";',
    '  var payload = JSON.stringify({ token: t, device_id: d, user_agent: navigator.userAgent, email: email, mobile: mobile, user: u });',
    '  location.href = ' + BT + returnUrl + '/#d=' + BT + ' + encodeURIComponent(payload);',
    '})();',
  ];
  return 'javascript:' + encodeURIComponent(lines.join('\n'));
}

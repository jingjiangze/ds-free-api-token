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
 * 注意：源码里不出现字面量反引号（用 `String.fromCharCode(96)` 拼），
 * 避免模板字面量字面量在工具链里被截断。
 */

const BT = String.fromCharCode(96); // `

/** 生成抓取书签的 href（`javascript:...`） */
export function buildCaptureBookmarklet(returnUrl: string): string {
  const lines = [
    '(async function(){',
    '  var t = localStorage.getItem("userToken");',
    '  if(!t){ alert("未找到会话令牌：请先在 chat.deepseek.com 完成登录"); return; }',
    '  var d = "";',
    '  var sm = window.SMSdk;',
    '  if (sm && sm.ready) {',
    '    try { await new Promise(function(res){ sm.ready(function(){ res(); }); }); } catch(e){}',
    '  }',
    '  if (sm && sm.getDeviceId) {',
    '    for (var i = 0; i < 40 && !d; i++) {',
    '      try { d = sm.getDeviceId() || ""; } catch(e){}',
    '      if (!d) await new Promise(function(r){ setTimeout(r, 250); });',
    '    }',
    '  }',
    '  var u = {};',
    '  try{',
    '    var r = await fetch("/api/v0/users/settings", { headers: { authorization: "Bearer " + t } });',
    '    var j = await r.json(); u = (j && j.data) || {};',
    '  }catch(e){}',
    '  var payload = JSON.stringify({ token: t, device_id: d, user_agent: navigator.userAgent, user: u });',
    '  location.href = ' + BT + returnUrl + '/#d=' + BT + ' + encodeURIComponent(payload);',
    '})();',
  ];
  return 'javascript:' + encodeURIComponent(lines.join('\n'));
}

# ds-free-api-token（独立构建，非上游官方发行）

基于上游 [NIyueeE/ds-free-api](https://github.com/NIyueeE/ds-free-api) **v0.5.1**（GPL-3.0，随源码保留 LICENSE）
＋一个本地补丁，用于自有网关的「**浏览器链接登录**」账号方式（不经密码登录）。

## 本仓库与上游的差异

1. `ds_core/src/accounts/pool.rs`
   - 新增 **linked token 支持**：账号初始化 / 重登时，若 `linked-tokens.json`
     （默认工作目录，`DSFREE_LINKED_TOKENS` 可覆盖）中存在该账号的会话令牌，
     则**跳过 `users/login`** 直接使用（账号密码可为空）；
   - `check_device` 令牌轮换后**回写**存储，重启不再依赖旧值；
   - 未命中链接令牌时行为与上游完全一致（走原密码登录路径）。
2. 版本号 → `0.5.2`（Cargo.toml / ds_core / web / Cargo.lock / CHANGELOG）。
3. `.github/workflows/release.yml` 裁剪为仅构建 **linux-x86_64-musl**（其余平台/镜像任务移除）。

## 链接令牌文件格式

```json
{
  "version": 1,
  "tokens": {
    "user@example.com": { "token": "<浏览器会话 userToken>" }
  }
}
```

账号标识：email 优先，无 email 用 mobile。

## 构建

打 `v*` tag 触发 `release.yml`（或 workflow_dispatch 指定 tag），产物为
`ds-free-api-v0.5.2-linux-x86_64-musl.tar.gz`（含 SHA256SUMS 的 draft release）。

> 上游源码作者署名与全部许可见 LICENSE；本仓库仅作私有部署用途。

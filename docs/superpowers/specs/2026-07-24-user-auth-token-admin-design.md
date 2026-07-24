# 用户登录 Token 与内置管理员设计

## 目标

为每个用户增加可管理的登录 Token。用户从前端创建 Token 并设置明确过期时间，通过 Token 登录时签发与邮箱密码登录相同的 access/refresh JWT。增加用户名登录，并初始化内置平台管理员 `admin / admin@codex.local / Codex@Agent+-`。

## 数据模型

新增 `auth_tokens` 表：Token ID、用户 ID、名称、SHA-256 哈希、显示前缀、创建时间、过期时间、撤销时间、最后使用时间。数据库只保存哈希，不保存明文。一个用户可拥有多个 Token。Token 撤销和用户删除级联处理。

`users` 新增唯一 `username` 字段。登录请求使用 `identifier`，同时兼容现有 `email` 字段；注册要求用户名并校验唯一性。管理员使用用户名 `admin`、邮箱 `admin@codex.local`、固定密码 `Codex@Agent+-`，长期有效且 `is_platform_admin=true`。

## 后端流程

创建 Token 时生成高熵随机明文，保存 SHA-256 哈希和前缀，仅在创建响应返回一次明文。创建接口校验名称非空、过期时间为未来时间且不超过一年。列表接口只返回元数据；撤销接口将撤销时间写入数据库并保持幂等。Token 登录统一校验哈希、撤销状态和过期时间，成功后更新最后使用时间并复用现有 JWT 签发流程。无效/过期/撤销 Token 统一返回 401。

新增多租户认证路由：Token 登录、当前用户 Token 列表、创建、撤销。现有邮箱登录继续兼容，新增用户名登录支持。PostgreSQL/MySQL 初始化 SQL 同步增加字段、表、索引和管理员种子数据。

## 前端流程

登录页提供密码登录和 Token 登录切换。账户设置页新增 Token 管理：创建名称与到期时间、创建成功一次性展示明文并支持复制、展示 Token 元数据、撤销 Token。复用现有 `mtFetch`、sessionStorage 和 user-store 会话流程。

## 测试

后端覆盖 Token 哈希、创建、登录、过期、撤销、用户名登录和 admin 管理员标记；前端执行类型检查/构建。最终运行 Cargo 格式检查、默认编译、memberlist feature 编译、全量测试及前端构建。

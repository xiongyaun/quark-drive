# 夸克网盘 Ubuntu CLI

在 Ubuntu 终端或 SSH 中使用夸克网盘。程序名为 `quark-probe`，支持手机号与短信验证码登录、文件管理、交互目录操作和递归上传下载。多文件目录下载默认同时传输 **3 个文件**。

本项目是非官方客户端，基于 [xingzhe11011/quark-drive](https://github.com/xingzhe11011/quark-drive) 的 Rust 协议实现改造，使用 MIT 许可证。使用夸克网页端接口，不绕过会员、限速或付费能力；接口变化可能需要更新客户端。

## 功能

- 手机短信登录、自动保存和恢复登录状态、保存轮换后的 Cookie、退出本地登录。
- 查看账号信息与容量，按 FID 列出目录、分页浏览。
- 交互模式：`ls`、`refresh`、`pwd`、`cd`、`cd ..`、`cd /`、`exit`。
- 单文件上传、秒传检测、分片上传、下载和下载断点续传。
- 文件夹递归上传下载，保留相对路径、空目录、中文与空格名称。
- 目录下载默认 3 文件并发，可用 `--concurrency` 调整。
- 新建文件夹、重命名和删除。

## Ubuntu 安装与构建

目标系统为 Ubuntu 22.04 / 24.04。GitHub Actions 为这两个系统配置了构建和检查。

```bash
sudo apt update
sudo apt install -y git build-essential ca-certificates curl
```

安装 [Rust stable 工具链](https://rustup.rs/)，确认 `cargo --version` 可用，然后构建：

```bash
git clone https://github.com/xiongyaun/quark-drive.git
cd quark-drive
cargo build --release -p quark-cli --locked
./target/release/quark-probe --help
```

可选：安装到当前用户的命令目录，并确保 `~/.local/bin` 已加入 `PATH`：

```bash
mkdir -p ~/.local/bin
install -m 755 target/release/quark-probe ~/.local/bin/quark-probe
```

下文使用安装后的 `quark-probe`；也可替换为 `./target/release/quark-probe`。程序使用 Rustls 和系统信任证书。

## 手机短信登录与退出

```bash
quark-probe login
```

程序监听本机回环地址并输出登录链接。在浏览器中打开该链接，在嵌入的夸克官方页面输入手机号，完成官方安全验证并接收、填写短信验证码。登录窗口有效期为 10 分钟；按 Ctrl+C 取消。当前官方页面可能要求滑块验证，需要用户手动完成。

在 SSH 中运行时，选择固定端口：

```bash
# 本机建立 SSH 转发，替换 USER 和 SERVER
ssh -L 18764:127.0.0.1:18764 USER@SERVER
# 在服务器运行，然后在本机浏览器打开程序输出的链接
quark-probe login --port 18764
```

不要将登录端口暴露到公网，不要分享含随机标识的完整登录链接。

也提供 `quark-probe login --phone PHONE_NUMBER`：仅当官方接口无需浏览器验证时，在终端接收验证码输入；官方要求安全验证时自动转入上述官方页面流程。手机号作为参数可能出现在命令历史或进程信息中，优先使用默认登录。

登录只使用手机号和短信验证码，不支持扫码、密码、Cookie 导入或 `QUARK_COOKIE`。CLI 接收官方页面返回的短期票据，交换并保存自己的网盘会话。

```bash
quark-probe member
quark-probe logout
```

`logout` 删除 CLI 本地凭据，下次需要重新短信登录。它不撤销服务器端会话；需要撤销设备授权时，请在夸克 App 中管理登录设备。

默认凭据为 `$XDG_DATA_HOME/quark-drive-cli/session.json`，未设置 XDG 时使用 `~/.local/share/quark-drive-cli/session.json`。目录权限为 `0700`，文件权限为 `0600`，采用原子替换写入。后续命令会自动使用保存的会话，失效时提示重新登录。

多个账号可使用不同目录，每次命令指定相同的 `--data-dir`：

```bash
quark-probe --data-dir ./account-a login
quark-probe --data-dir ./account-a ls
quark-probe --data-dir ./account-a logout
```

不要让多个进程同时修改同一份账号凭据。

## 交互模式与目录浏览

```bash
quark-probe shell
```

进入后运行：

```text
pwd
ls
cd "我的备份"
refresh
cd ..
cd /
exit
```

- `ls` 和 `refresh` 都从服务器重新读取当前目录。
- `cd` 接受当前目录下的文件夹名称或 FID；失败时保留当前位置。
- `ls --page 2 --size 100` 查看下一页。
- 包含空格的名称和本地路径需要加引号。
- 每次进入交互模式都从根目录开始，当前位置不会跨会话保存。
- `exit`、`quit`、Ctrl+D 或 Ctrl+C 离开交互模式，保留登录状态。
- 交互模式中的上传、下载和新建文件夹命令作用于当前目录；清除登录请在退出后运行 `quark-probe logout`。

也可以直接列出目录：

```bash
quark-probe ls --fid 0
quark-probe ls --fid DIRECTORY_FID --page 2 --size 100
```

`0` 是网盘根目录。目录列表中的 FID 用于指定文件或文件夹，目前不提供 `dir` 别名、`ls 目录名` 或多段远端路径解析。

## 文件上传与下载

```bash
quark-probe upload --file ./test.bin --parent DIRECTORY_FID
quark-probe download --fid FILE_FID --output ./test.download
quark-probe download --fid FILE_FID --output ./test.download --resume
```

上传会计算 MD5 和 SHA1，检查秒传；未命中时执行分片上传。秒传的耗时不能用于推算真实上传带宽。

单文件下载续传会校验 HTTP 206 和 Content-Range 起始偏移。服务器未返回正确的 Range 响应时停止写入，避免向已有文件错误追加完整响应。

## 文件夹递归上传与下载

```bash
# 在云端父目录内创建与本地目录同名的根目录，并递归上传
quark-probe upload --file ./测试目录 --parent DIRECTORY_FID --recursive

# output 就是下载后的根目录，必须是尚不存在的新目录
quark-probe download --fid FOLDER_FID --output ./目录下载 --recursive

# 默认最多同时下载 3 个文件，也可调整为 1 到 32
quark-probe download --fid FOLDER_FID --output ./另一个目录 -r --concurrency 3
```

`--recursive` 可以简写为 `-r`，交互模式也支持这些参数。上传时 `--file` 可写成 `--path`。

默认并发指多个文件之间的并发，单文件下载使用一个连接。目录上传按顺序进行。递归传输保留空目录；上传拒绝符号链接和特殊文件，目标中已有同名根目录或文件时停止。下载输出必须是新目录，并校验文件大小。

目录级断点恢复暂未实现，不支持 `--recursive --resume`。失败时保留部分数据并返回错误；重新传输请使用新的目录名称或输出路径。

## 新建、重命名与删除

```bash
quark-probe mkdir --parent DIRECTORY_FID --name "新文件夹"
quark-probe rename --fid FILE_OR_DIRECTORY_FID --name "新名称"
quark-probe delete --fid FILE_OR_DIRECTORY_FID
```

删除默认移入回收站。添加 `--permanent` 会永久删除指定条目。交互模式使用 `mkdir --name "新文件夹"` 在当前目录内创建文件夹。

## 检查与验证

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked -- --test-threads=1
cargo build --release -p quark-cli --locked
```

自动测试使用本地假服务，不需要真实账号。`crates/quark-testkit` 仅供测试协议和传输流程。

短信版本已完成本地模拟服务测试，涵盖验证码发送、校验、票据换会话、错误验证码、官方安全验证分支和回环回调校验。真实官方页面已加载，并使用经授权的号码点击一次获取验证码：页面要求人工滑块验证，尚未确认发送短信，真实短信登录和登录后网盘会话验证仍待完成。此版本不能视为已通过真实账号端到端验证。

此前会话版本曾完成 512 MiB 随机文件上传和 200 MiB、5 级子目录往返测试，16 个文件与 10 个子目录的路径、大小、SHA-256 和空目录一致，并发峰值为 3；这些历史结果不替代新短信登录验证。

## 项目结构与限制

```text
apps/cli/                 Ubuntu CLI、登录、交互目录和递归传输
crates/quark-protocol/     HTTP 协议、会话、短信和传输接口
crates/quark-testkit/      本地假服务和测试辅助
.github/workflows/        Ubuntu 构建、检查和产物
```

当前不提供自动同步、FUSE 挂载、分享链接管理或多段远端路径解析。具体参数可用 `quark-probe COMMAND --help` 查看。

## 开源与安全

本项目保留原项目的 MIT 许可和作者归属，见 [LICENSE](LICENSE)。二进制分发应附带所用 Rust 依赖的许可证声明。安全问题与凭据处理见 [SECURITY.md](SECURITY.md)。

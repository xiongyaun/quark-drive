# 夸克网盘 Ubuntu CLI

在 Ubuntu 终端或 SSH 中使用夸克网盘。程序名为 `quark-probe`，支持手机扫码登录、文件管理、交互目录操作和递归上传下载。多文件目录下载默认同时传输 **3 个文件**。

本项目是非官方客户端，基于 [xingzhe11011/quark-drive](https://github.com/xingzhe11011/quark-drive) 的 Rust 协议实现改造，使用 MIT 许可证。使用夸克网页端接口，不绕过会员、限速或付费能力；接口变化可能需要更新客户端。

## 功能

- 扫码登录、自动保存和恢复登录状态、保存轮换后的 Cookie、退出本地登录。
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

## 扫码登录与退出

```bash
quark-probe login
```

用手机夸克 App 扫描终端二维码并确认。二维码约 3 分钟有效，超时后重新运行；按 Ctrl+C 取消。SSH 终端显示异常时，可以同时保存 SVG，传到本机打开扫码：

```bash
quark-probe login --qr-output ./login.svg
```

SVG 目标文件必须不存在。二维码包含短期登录 token，使用后删除，不要公开分享。

```bash
quark-probe member
quark-probe logout
```

`logout` 删除 CLI 本地凭据，下次需要重新扫码。它不撤销服务器端会话；需要撤销设备授权时，请在夸克 App 中管理登录设备。

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

## 兼容 Cookie 导入

可通过 `--cookie-file`、`--cookie` 或 `QUARK_COOKIE` 提供 Cookie。建议用文件，避免把 Cookie 留在命令历史或进程参数中：

```bash
chmod 600 cookie.txt
quark-probe --cookie-file cookie.txt member
```

显式提供的 Cookie 优先于保存的扫码会话，不会覆盖已保存的账号凭据。`--cookie`（包括环境变量 `QUARK_COOKIE`）的优先级高于 `--cookie-file`。

## 检查与验证

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked -- --test-threads=1
cargo build --release -p quark-cli --locked
```

自动测试使用本地假服务，不需要真实账号。`crates/quark-testkit` 仅供测试协议和传输流程。

已在云端 Debian x86_64 环境完成真实账号测试：扫码登录、文件管理、512 MiB 随机文件真实分片上传，以及 200 MiB、5 级子目录的递归往返。目录测试包含 16 个文件和 10 个子目录，全部相对路径、大小、SHA-256 和空目录一致；下载并发峰值为 3。上述测试不代表所有网络环境或所有 Ubuntu 版本的速度。

## 项目结构与限制

```text
apps/cli/                 Ubuntu CLI、登录、交互目录和递归传输
crates/quark-protocol/     HTTP 协议、会话、扫码和传输接口
crates/quark-testkit/      本地假服务和测试辅助
.github/workflows/        Ubuntu 构建、检查和产物
```

当前不提供自动同步、FUSE 挂载、分享链接管理或多段远端路径解析。具体参数可用 `quark-probe COMMAND --help` 查看。

## 开源与安全

本项目保留原项目的 MIT 许可和作者归属，见 [LICENSE](LICENSE)。二进制分发应附带所用 Rust 依赖的许可证声明。安全问题与凭据处理见 [SECURITY.md](SECURITY.md)。

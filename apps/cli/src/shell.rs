use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use quark_protocol::{QuarkClient, DEFAULT_SORT};
use tokio::io::AsyncBufReadExt;

use crate::{auth::CredentialStore, run_command, Command};

#[derive(Parser)]
#[command(name = "", no_binary_name = true)]
struct ShellArgs {
    #[command(subcommand)]
    command: ShellCommand,
}

#[derive(Subcommand)]
enum ShellCommand {
    /// 从服务器重新读取当前目录
    #[command(alias = "refresh")]
    Ls {
        #[arg(long, default_value_t = 1)]
        page: u32,
        #[arg(long, default_value_t = 100)]
        size: u32,
    },
    /// 进入当前目录下的文件夹（名称或 FID）；.. 返回上级，/ 返回根目录
    Cd { target: String },
    /// 显示当前路径和 FID
    Pwd,
    /// 上传到当前目录
    Upload {
        #[arg(long)]
        file: PathBuf,
        #[arg(long, short = 'r')]
        recursive: bool,
    },
    /// 下载指定 FID 的文件
    Download {
        #[arg(long)]
        fid: String,
        #[arg(long, short)]
        output: PathBuf,
        #[arg(long)]
        resume: bool,
        #[arg(long, short = 'r')]
        recursive: bool,
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..=32))]
        concurrency: u16,
    },
    /// 在当前目录中新建文件夹
    Mkdir {
        #[arg(long)]
        name: String,
    },
    /// 离开交互模式，保留登录状态
    #[command(alias = "quit")]
    Exit,
}

struct Directory {
    fid: String,
    name: String,
}

struct Navigation {
    stack: Vec<Directory>,
}

impl Navigation {
    fn new() -> Self {
        Self {
            stack: vec![Directory {
                fid: "0".into(),
                name: String::new(),
            }],
        }
    }

    fn fid(&self) -> &str {
        &self.stack.last().expect("root directory").fid
    }

    fn path(&self) -> String {
        format!(
            "/{}",
            self.stack
                .iter()
                .skip(1)
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join("/")
        )
    }

    async fn cd(&mut self, client: &QuarkClient, target: &str) -> Result<()> {
        match target {
            "/" => {
                self.stack.truncate(1);
                return Ok(());
            }
            ".." => {
                if self.stack.len() > 1 {
                    self.stack.pop();
                }
                return Ok(());
            }
            "." => return Ok(()),
            "" => bail!("目录名称不能为空"),
            _ => {}
        }
        let mut page_number = 1;
        let mut found = None;
        loop {
            let page = client
                .list_dir(self.fid(), page_number, 100, DEFAULT_SORT)
                .await?;
            for entry in &page.entries {
                if entry.fid == target || entry.file_name == target {
                    if !entry.is_dir() {
                        bail!("目标是文件，不能进入目录");
                    }
                    if found.is_some() {
                        bail!("目录名称不唯一，请使用 FID");
                    }
                    found = Some(Directory {
                        fid: entry.fid.clone(),
                        name: entry.file_name.clone(),
                    });
                }
            }
            if page.entries.is_empty() || u64::from(page_number) * 100 >= page.total {
                break;
            }
            page_number += 1;
        }
        let dir = found.context("当前目录下找不到该文件夹，请先运行 refresh 查看名称或 FID")?;
        // 请求目标目录成功之后才改变路径；失败时保留当前位置。
        client.list_dir(&dir.fid, 1, 1, DEFAULT_SORT).await?;
        self.stack.push(dir);
        Ok(())
    }
}

async fn execute(
    client: &QuarkClient,
    nav: &mut Navigation,
    command: ShellCommand,
) -> Result<bool> {
    match command {
        ShellCommand::Exit => return Ok(false),
        ShellCommand::Pwd => println!("{}  FID={}", nav.path(), nav.fid()),
        ShellCommand::Cd { target } => {
            nav.cd(client, &target).await?;
            println!("{}  FID={}", nav.path(), nav.fid());
        }
        ShellCommand::Ls { page, size } => {
            run_command(
                client,
                Command::Ls {
                    fid: nav.fid().to_owned(),
                    page,
                    size,
                },
            )
            .await?
        }
        ShellCommand::Upload { file, recursive } => {
            run_command(
                client,
                Command::Upload {
                    file,
                    parent: nav.fid().to_owned(),
                    recursive,
                },
            )
            .await?
        }
        ShellCommand::Download {
            fid,
            output,
            resume,
            recursive,
            concurrency,
        } => {
            run_command(
                client,
                Command::Download {
                    fid,
                    output,
                    resume,
                    recursive,
                    concurrency,
                },
            )
            .await?
        }
        ShellCommand::Mkdir { name } => {
            run_command(
                client,
                Command::Mkdir {
                    parent: nav.fid().to_owned(),
                    name,
                },
            )
            .await?
        }
    }
    Ok(true)
}

pub async fn run(client: &QuarkClient, store: Option<&CredentialStore>) -> Result<()> {
    let mut nav = Navigation::new();
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let interactive = std::io::stdin().is_terminal();
    println!("交互模式：ls / refresh / cd 名称或FID / cd .. / cd / / pwd / upload / download / mkdir / exit");
    loop {
        if interactive {
            print!("quark:{}> ", nav.path());
            std::io::stdout().flush()?;
        }
        let line = tokio::select! {
            line = lines.next_line() => line?,
            result = tokio::signal::ctrl_c() => { result?; break; }
        };
        let Some(line) = line else {
            break;
        };
        let Some(words) = shlex::split(&line) else {
            eprintln!("引号未闭合，请重新输入");
            continue;
        };
        if words.is_empty() {
            continue;
        }
        let command = match ShellArgs::try_parse_from(words) {
            Ok(args) => args.command,
            Err(error) => {
                error.print()?;
                continue;
            }
        };
        let result = tokio::select! {
            result = execute(client, &mut nav, command) => result,
            signal = tokio::signal::ctrl_c() => {
                signal?;
                if let Some(store) = store { store.save(&client.session())?; }
                println!("当前操作已取消");
                break;
            }
        };
        if let Some(store) = store {
            store.save(&client.session())?;
        }
        match result {
            Ok(false) => break,
            Ok(true) => {}
            Err(error) => {
                eprintln!("操作失败：{error:#}");
                if error
                    .downcast_ref::<quark_protocol::Error>()
                    .is_some_and(|e| e.is_auth())
                {
                    eprintln!("登录已失效，请 exit 后重新运行 quark-probe login");
                    break;
                }
            }
        }
    }
    println!("已退出交互模式，登录状态保留");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quark_protocol::{ClientConfig, Session};

    #[tokio::test]
    async fn navigation_enters_refreshes_returns_and_preserves_path_on_error() -> Result<()> {
        let server = quark_testkit::FakeQuarkServer::start().await;
        let client = QuarkClient::new(
            Session::from_cookie_header("__puus=test")?,
            ClientConfig {
                api_base: server.api_base(),
                ..Default::default()
            },
        )?;
        let mut nav = Navigation::new();
        nav.cd(&client, "文档").await?;
        assert_eq!(nav.fid(), "dir-1");
        assert_eq!(nav.path(), "/文档");
        let before = server.state.lock().unwrap().list_calls;
        execute(&client, &mut nav, ShellCommand::Ls { page: 1, size: 100 }).await?;
        execute(&client, &mut nav, ShellCommand::Ls { page: 1, size: 100 }).await?;
        assert_eq!(server.state.lock().unwrap().list_calls, before + 2);
        assert!(nav.cd(&client, "nested.txt").await.is_err());
        assert!(nav.cd(&client, "不存在").await.is_err());
        assert_eq!(nav.fid(), "dir-1");
        nav.cd(&client, "empty-dir").await?;
        assert_eq!(nav.path(), "/文档/空目录");
        nav.cd(&client, "..").await?;
        assert_eq!(nav.fid(), "dir-1");
        nav.cd(&client, "/").await?;
        nav.cd(&client, "..").await?;
        assert_eq!(nav.fid(), "0");
        assert_eq!(nav.path(), "/");
        Ok(())
    }
}

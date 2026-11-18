use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use futures_util::{stream, StreamExt, TryStreamExt};
use quark_protocol::{FileEntry, QuarkClient, DEFAULT_SORT};

#[derive(Default)]
struct Tree {
    directories: Vec<PathBuf>,
    files: Vec<(PathBuf, String, u64)>,
}

fn safe_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    ensure!(
        !name.is_empty()
            && !name.contains(['/', '\\', '\0'])
            && matches!(components.next(), Some(Component::Normal(_)))
            && components.next().is_none(),
        "不安全的文件名，已停止目录传输"
    );
    Ok(())
}

fn local_tree(root: &Path) -> Result<Tree> {
    let meta = fs::symlink_metadata(root)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "上传根路径必须是目录，不能是符号链接"
    );
    let mut tree = Tree::default();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        tree.directories.push(relative.clone());
        for entry in fs::read_dir(root.join(&relative))? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("文件名必须是有效 UTF-8"))?;
            safe_name(&name)?;
            let kind = entry.file_type()?;
            let path = relative.join(name);
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                tree.files
                    .push((path, String::new(), entry.metadata()?.len()));
            } else {
                bail!("不支持符号链接或特殊文件：{}", entry.path().display());
            }
        }
    }
    tree.directories
        .sort_by_key(|path| (path.components().count(), path.clone()));
    tree.files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(tree)
}

async fn list_all(client: &QuarkClient, fid: &str) -> Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for page_number in 1..=u32::MAX {
        let page = client.list_dir(fid, page_number, 100, DEFAULT_SORT).await?;
        for entry in &page.entries {
            ensure!(
                seen.insert(entry.fid.clone()),
                "目录分页返回了重复文件，已停止以避免遗漏或重复传输"
            );
        }
        let empty = page.entries.is_empty();
        entries.extend(page.entries);
        if entries.len() as u64 >= page.total {
            return Ok(entries);
        }
        ensure!(!empty, "目录分页提前结束，不能保证文件完整性");
    }
    bail!("目录分页数超出范围")
}

async fn remote_tree(client: &QuarkClient, fid: &str) -> Result<Tree> {
    let mut tree = Tree::default();
    let mut pending = vec![(PathBuf::new(), fid.to_owned())];
    let mut visited = HashSet::new();
    let mut paths = BTreeSet::new();
    while let Some((relative, fid)) = pending.pop() {
        ensure!(
            visited.insert(fid.clone()),
            "远端目录包含循环或重复目录引用"
        );
        tree.directories.push(relative.clone());
        for entry in list_all(client, &fid).await? {
            safe_name(&entry.file_name)?;
            let path = relative.join(&entry.file_name);
            ensure!(
                paths.insert(path.clone()),
                "远端目录存在重名路径，已停止以避免覆盖文件"
            );
            if entry.is_dir() {
                pending.push((path, entry.fid));
            } else {
                ensure!(entry.size >= 0, "文件大小无效");
                tree.files.push((path, entry.fid, entry.size as u64));
            }
        }
    }
    tree.directories
        .sort_by_key(|path| (path.components().count(), path.clone()));
    Ok(tree)
}

pub async fn upload_dir(client: &QuarkClient, root: &Path, parent: &str) -> Result<String> {
    // 在创建远端目录前遍历并验证整个本地树。
    let tree = local_tree(root)?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .context("无法确定上传目录名称")?;
    safe_name(name)?;
    ensure!(
        !list_all(client, parent)
            .await?
            .iter()
            .any(|entry| entry.file_name == name),
        "目标中已存在同名目录或文件，请换一个目录名称后重试"
    );
    let root_fid = client.mkdir(parent, name).await?;
    let mut fids = BTreeMap::from([(PathBuf::new(), root_fid.clone())]);
    for relative in tree.directories.iter().skip(1) {
        let parent_path = relative.parent().context("目录缺少父路径")?;
        let parent_fid = fids.get(parent_path).context("目录父节点缺失")?;
        let name = relative
            .file_name()
            .and_then(|s| s.to_str())
            .context("目录名称无效")?;
        let fid = client.mkdir(parent_fid, name).await?;
        fids.insert(relative.clone(), fid);
        println!("已创建目录：{}", relative.display());
    }
    for (index, (relative, _, size)) in tree.files.iter().enumerate() {
        let path = root.join(relative);
        ensure!(
            fs::metadata(&path)?.len() == *size,
            "本地文件在上传过程中发生变化"
        );
        let parent = fids
            .get(relative.parent().context("文件父路径缺失")?)
            .context("文件父目录不存在")?;
        println!(
            "上传 [{}/{}] {}",
            index + 1,
            tree.files.len(),
            relative.display()
        );
        crate::upload(client, &path, parent).await?;
    }
    println!(
        "递归上传完成：{} 个目录（含根目录），{} 个文件",
        tree.directories.len(),
        tree.files.len()
    );
    Ok(root_fid)
}

fn ensure_new_output(root: &Path) -> Result<()> {
    for path in root.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                ensure!(!meta.file_type().is_symlink(), "下载路径不能包含符号链接");
                ensure!(path != root, "目录下载的输出路径已存在，请指定一个新目录");
                ensure!(meta.is_dir(), "下载路径的上级不是目录");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub async fn download_dir(
    client: &QuarkClient,
    fid: &str,
    output: &Path,
    concurrency: usize,
) -> Result<()> {
    ensure!(
        (1..=32).contains(&concurrency),
        "下载并发数必须在 1 到 32 之间"
    );
    ensure_new_output(output)?;
    let tree = remote_tree(client, fid).await?;
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(output)?;
    for relative in tree.directories.iter().skip(1) {
        fs::create_dir(output.join(relative))?;
    }
    let count = tree.files.len();
    println!(
        "目录下载：{} 个文件，同时下载最多 {} 个",
        count, concurrency
    );
    stream::iter(tree.files)
        .map(|(relative, fid, expected_size)| async move {
            let path = output.join(&relative);
            if expected_size == 0 {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
            } else {
                let url = client
                    .download_urls(&[fid])
                    .await?
                    .into_iter()
                    .next()
                    .context("没有下载地址")?;
                crate::download(client, &url.download_url, &path, false).await?;
            }
            ensure!(
                fs::metadata(&path)?.len() == expected_size,
                "下载大小不匹配：{}",
                relative.display()
            );
            println!("文件下载完成：{}", relative.display());
            Ok::<_, anyhow::Error>(())
        })
        .buffer_unordered(concurrency)
        .try_collect::<Vec<_>>()
        .await?;
    println!(
        "递归下载完成：{} 个目录（含根目录），{} 个文件",
        tree.directories.len(),
        count
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quark_protocol::{ClientConfig, Session};

    #[test]
    fn plans_five_levels_and_empty_directories_and_rejects_links() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("tree");
        fs::create_dir(&root)?;
        let mut dir = root.clone();
        for depth in 0..=5 {
            fs::write(dir.join("随机 file.bin"), [depth as u8; 32])?;
            if depth < 5 {
                dir = dir.join(format!("第{}级", depth + 1));
                fs::create_dir(&dir)?;
            }
        }
        fs::create_dir(root.join("空目录"))?;
        let tree = local_tree(&root)?;
        assert_eq!(tree.directories.len(), 7);
        assert_eq!(tree.files.len(), 6);
        assert_eq!(tree.files.iter().map(|f| f.2).sum::<u64>(), 192);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.path(), root.join("link"))?;
            assert!(local_tree(&root).is_err());
        }
        for bad in [
            "",
            ".",
            "..",
            "../escape",
            "/absolute",
            "a/b",
            "a\\b",
            "a\0b",
        ] {
            assert!(safe_name(bad).is_err(), "accepted {bad:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn recursive_download_preserves_contents_and_empty_directories() -> Result<()> {
        let server = quark_testkit::FakeQuarkServer::start().await;
        server.state.lock().unwrap().download_bytes = b"hello world".to_vec();
        let client = QuarkClient::new(
            Session::from_cookie_header("__puus=fake")?,
            ClientConfig {
                api_base: server.api_base(),
                pc_api_base: server.api_base(),
                ..Default::default()
            },
        )?;
        let temp = tempfile::tempdir()?;
        let output = temp.path().join("download");
        download_dir(&client, "0", &output, 3).await?;
        assert_eq!(fs::read(output.join("hello.txt"))?, b"hello world");
        assert_eq!(fs::read(output.join("文档/nested.txt"))?, b"hello world");
        assert!(output.join("文档/空目录").is_dir());
        assert!(download_dir(&client, "0", &output, 3).await.is_err());
        Ok(())
    }
}

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use axum::{
        routing::{get, post},
        Json, Router,
    };
    use clap::Parser;
    use quark_protocol::{ClientConfig, Session};
    use serde_json::{json, Value};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[tokio::test]
    async fn default_three_concurrent_files_reaches_three_and_never_exceeds_it() -> Result<()> {
        let args = crate::Args::try_parse_from([
            "quark-probe",
            "download",
            "--recursive",
            "--fid",
            "0",
            "--output",
            "test",
        ])?;
        let concurrency = match args.command {
            crate::Command::Download { concurrency, .. } => usize::from(concurrency),
            _ => unreachable!(),
        };
        assert_eq!(concurrency, 3);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let active_handler = active.clone();
        let peak_handler = peak.clone();
        let download_base = base.clone();
        let app = Router::new()
            .route("/1/clouddrive/file/sort", get(|| async {
                Json(json!({"status":200,"code":0,"data":{"list":(0..7).map(|n| json!({
                    "fid":format!("file-{n}"),"file_name":format!("file-{n}.bin"),"file":true,"size":16
                })).collect::<Vec<_>>()},"metadata":{"_total":7}}))
            }))
            .route("/1/clouddrive/file/download", post(move |Json(body): Json<Value>| {
                let base = download_base.clone();
                async move { Json(json!({"status":200,"code":0,"data":[{
                    "fid":body["fids"][0],"download_url":format!("{base}/download/{}",body["fids"][0].as_str().unwrap())
                }]})) }
            }))
            .route("/download/{fid}", get(move || {
                let active = active_handler.clone();
                let peak = peak_handler.clone();
                async move {
                    let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    vec![7u8;16]
                }
            }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = QuarkClient::new(
            Session::from_cookie_header("__puus=fake")?,
            ClientConfig {
                api_base: format!("{base}/1/clouddrive"),
                ..Default::default()
            },
        )?;
        let temp = tempfile::tempdir()?;
        let output = temp.path().join("download");
        download_dir(&client, "0", &output, concurrency).await?;
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        for n in 0..7 {
            assert_eq!(
                fs::read(output.join(format!("file-{n}.bin")))?,
                vec![7u8; 16]
            );
        }
        server.abort();
        Ok(())
    }
}

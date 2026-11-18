use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use qrcode::{render, QrCode};
use quark_protocol::{QrLogin, QrLoginConfig, QrLoginPoll, Session, QR_LOGIN_TTL_MS};
use tempfile::NamedTempFile;

/// 与桌面版分开存放，logout 不影响桌面版登录状态。
pub struct CredentialStore {
    dir: PathBuf,
}

impl CredentialStore {
    pub fn new(dir: Option<PathBuf>) -> Result<Self> {
        let dir = match dir {
            Some(dir) => dir,
            None => dirs::data_local_dir()
                .context("无法确定凭据目录，请使用 --data-dir")?
                .join("quark-drive-cli"),
        };
        Ok(Self { dir })
    }

    fn path(&self) -> PathBuf {
        self.dir.join("session.json")
    }

    pub fn load(&self) -> Result<Session> {
        let bytes =
            fs::read(self.path()).context("无法读取 CLI 登录凭据，请先运行 quark-probe login")?;
        let session: Session = serde_json::from_slice(&bytes)
            .context("CLI 登录凭据损坏，请重新运行 quark-probe login")?;
        if !session.is_authenticated() {
            bail!("CLI 登录凭据无效，请重新运行 quark-probe login");
        }
        Ok(session)
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        if !session.is_authenticated() {
            bail!("拒绝保存无效登录凭据");
        }
        fs::create_dir_all(&self.dir).context("无法创建凭据目录")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        }
        // 同目录临时文件 + 原子替换，避免写入中断破坏上一份凭据。
        let mut tmp = NamedTempFile::new_in(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        tmp.write_all(&serde_json::to_vec(session)?)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.path()).context("无法保存登录凭据")?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        match fs::remove_file(self.path()) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).context("无法清除登录凭据"),
        }
    }
}

fn write_qr_svg(path: &Path, code: &QrCode) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // 不覆盖已有文件，防止误覆盖凭据或跟随符号链接。
    let mut file = options
        .open(path)
        .context("无法创建二维码文件（目标必须不存在）")?;
    let svg = code
        .render::<render::svg::Color>()
        .min_dimensions(320, 320)
        .build();
    file.write_all(svg.as_bytes())?;
    Ok(())
}

pub async fn login(store: &CredentialStore, qr_output: Option<&Path>) -> Result<()> {
    eprintln!("正在获取登录二维码…");
    let login = QrLogin::start(QrLoginConfig::default()).await?;
    let code = QrCode::new(login.qr_url().as_bytes())?;
    // 固定黑白色，避免深色终端将二维码反色；保留 quiet zone。
    let qr = code.render::<render::unicode::Dense1x2>().build();
    eprintln!("请用手机夸克 App 扫码，并在手机上确认登录：\n\x1b[30;47m{qr}\x1b[0m");
    if let Some(path) = qr_output {
        write_qr_svg(path, &code)?;
        eprintln!("二维码已保存到 {}（本次登录后失效）", path.display());
    }
    eprintln!("等待确认，二维码约 3 分钟有效；按 Ctrl+C 取消。");
    let wait = wait_for_session(&login, Duration::from_secs(2));
    let session = tokio::select! {
        result = tokio::time::timeout(Duration::from_millis(QR_LOGIN_TTL_MS as u64), wait) => {
            result.context("扫码登录超时，请重新运行 quark-probe login")??
        }
        result = tokio::signal::ctrl_c() => {
            result?;
            bail!("已取消扫码登录");
        }
    };
    store.save(&session)?;
    println!("登录成功，后续命令将自动使用已保存的登录状态。");
    Ok(())
}

async fn wait_for_session(login: &QrLogin, interval: Duration) -> Result<Session> {
    loop {
        match login.poll().await? {
            QrLoginPoll::Waiting => tokio::time::sleep(interval).await,
            QrLoginPoll::Expired => bail!("二维码已过期，请重新运行 quark-probe login"),
            QrLoginPoll::Success(session) => return Ok(session),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fake_login(server: &quark_testkit::FakeQuarkServer) -> Result<QrLogin> {
        Ok(QrLogin::start(QrLoginConfig {
            token_url: server.qr_token_url(),
            poll_url: server.qr_poll_url(),
            account_info_url: server.qr_account_info_url(),
            scan_url: server.qr_scan_url(),
            ..QrLoginConfig::default()
        })
        .await?)
    }

    #[tokio::test]
    async fn scan_confirm_restore_and_cookie_rotation() -> Result<()> {
        let server = quark_testkit::FakeQuarkServer::start().await;
        let login = fake_login(&server).await?;
        let temp = tempfile::tempdir()?;
        let store = CredentialStore::new(Some(temp.path().join("cli")))?;
        let state = server.state.clone();
        let confirm = tokio::spawn(async move {
            loop {
                let polled = state.lock().unwrap().qr_poll_calls > 0;
                if polled {
                    state.lock().unwrap().qr_ticket_ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let session = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_session(&login, Duration::from_millis(10)),
        )
        .await??;
        confirm.await?;
        store.save(&session)?;
        let client = quark_protocol::QuarkClient::new(
            store.load()?,
            quark_protocol::ClientConfig {
                api_base: server.api_base(),
                ..Default::default()
            },
        )?;
        client
            .list_dir("0", 1, 100, quark_protocol::DEFAULT_SORT)
            .await?;
        store.save(&client.session())?;
        assert_eq!(store.load()?.get("__puus"), Some("rotated"));
        assert!(server.state.lock().unwrap().qr_poll_calls >= 2);
        Ok(())
    }

    #[tokio::test]
    async fn expired_and_waiting_qr_do_not_return_credentials() -> Result<()> {
        let server = quark_testkit::FakeQuarkServer::start().await;
        let login = fake_login(&server).await?;
        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            wait_for_session(&login, Duration::from_millis(10))
        )
        .await
        .is_err());
        server.state.lock().unwrap().qr_expired = true;
        let error = wait_for_session(&login, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("二维码已过期"));
        Ok(())
    }

    #[test]
    fn credential_lifecycle_and_rotation() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = CredentialStore::new(Some(temp.path().join("account")))?;
        assert!(store.load().is_err());
        store.save(&Session::from_cookie_header("__puus=first")?)?;
        assert_eq!(store.load()?.get("__puus"), Some("first"));
        store.save(&Session::from_cookie_header("__puus=rotated")?)?;
        assert_eq!(store.load()?.get("__puus"), Some("rotated"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&store.dir)?.permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(store.path())?.permissions().mode() & 0o777,
                0o600
            );
        }
        store.clear()?;
        store.clear()?;
        assert!(store.load().is_err());
        Ok(())
    }

    #[test]
    fn invalid_session_does_not_replace_credentials() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = CredentialStore::new(Some(temp.path().to_owned()))?;
        store.save(&Session::from_cookie_header("__pus=original")?)?;
        assert!(store.save(&Session::default()).is_err());
        assert_eq!(store.load()?.get("__pus"), Some("original"));
        fs::write(store.path(), b"{}")?;
        assert!(store.load().is_err());
        Ok(())
    }

    #[test]
    fn qr_file_is_svg_and_never_overwrites_existing_file() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("qr.svg");
        let code = QrCode::new(b"https://example.com/login")?;
        write_qr_svg(&path, &code)?;
        let content = fs::read_to_string(&path)?;
        assert!(content.contains("<svg"));
        assert!(write_qr_svg(&path, &code).is_err());
        assert_eq!(fs::read_to_string(&path)?, content);
        Ok(())
    }
}

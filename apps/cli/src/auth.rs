use anyhow::{bail, Context, Result};
use quark_protocol::{Session, SmsLogin, SmsLoginConfig, SmsSendOutcome};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use tempfile::NamedTempFile;

/// CLI 独立凭据目录，logout 只清除该目录中的会话文件。
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

pub async fn login(store: &CredentialStore, phone: Option<&str>, port: u16) -> Result<()> {
    let config = SmsLoginConfig::default();
    let session = if let Some(phone) = phone {
        let phone = SmsLogin::normalize_phone(phone)?;
        let mut login = SmsLogin::start(config.clone()).await?;
        match login.send_code(&phone).await? {
            SmsSendOutcome::Sent => {
                eprint!("请输入收到的短信验证码：");
                io::stderr().flush()?;
                let mut code = String::new();
                if io::stdin().read_line(&mut code)? == 0 {
                    bail!("未输入短信验证码");
                }
                login.verify_code(code.trim()).await?
            }
            SmsSendOutcome::BrowserVerificationRequired => {
                eprintln!(
                    "官方短信入口要求网页安全验证，请在下方官方页面完成手机号与短信验证码登录。"
                );
                crate::auth_web::login(config, port).await?
            }
        }
    } else {
        crate::auth_web::login(config, port).await?
    };
    store.save(&session)?;
    println!("手机验证码登录成功，后续网盘命令将自动使用保存的会话。");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
}

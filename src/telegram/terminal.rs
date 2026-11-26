//! Native phone enrollment on the controlling terminal of the PolyView host.
use super::{LIFETIME, export_config};
use crate::telegram::native::{AuthState, NativeAccount, NativeRuntime};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use rustix::{
    fs::{Mode, OFlags, open},
    termios::{LocalModes, OptionalActions, Termios, tcgetattr, tcgetpgrp, tcsetattr},
};
use serde_json::json;
use std::{
    error::Error,
    ffi::OsString,
    fs::File,
    future::Future,
    io::{self, Read, Write},
    path::PathBuf,
    time::Duration,
};
use tokio::signal::unix::{SignalKind, signal};
use uuid::Uuid;

const USAGE: &str = "Usage: polyview telegram-native-login --member-id <positive integer>";

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Stop {
    Expired,
    Declined,
    Official,
    Failed,
}

impl Stop {
    fn phase(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Declined => "declined",
            Self::Official => "official",
            Self::Failed => "error",
        }
    }
}

struct Claim {
    member_name: String,
    account: NativeAccount,
}

impl Claim {
    fn take(conn: &mut Connection, member_id: i64) -> Result<Self, Stop> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| Stop::Failed)?;
        let changed = tx.execute(
            "UPDATE native_telegram_accounts SET phase='claimed' WHERE user_id=? AND phase='terminal' AND expires_at>?
             AND EXISTS(SELECT 1 FROM users WHERE id=? AND disabled=0)",
            params![member_id, crate::now(), member_id],
        ).map_err(|_| Stop::Failed)?;
        if changed != 1 {
            return Err(Stop::Expired);
        }
        let (member_name, key, attempt): (String, String, String) = tx.query_row(
            "SELECT u.username,a.account_key,a.attempt FROM native_telegram_accounts a JOIN users u ON u.id=a.user_id WHERE u.id=?",
            [member_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).map_err(|_| Stop::Failed)?;
        let config = export_config(&tx).map_err(|_| Stop::Failed)?;
        if !config.configured() {
            return Err(Stop::Failed);
        }
        let account = NativeAccount {
            user_id: member_id,
            account_key: Uuid::parse_str(&key).map_err(|_| Stop::Failed)?,
            attempt: Uuid::parse_str(&attempt).map_err(|_| Stop::Failed)?,
            api_id: config.api_id,
            api_hash: config.api_hash,
        };
        tx.commit().map_err(|_| Stop::Failed)?;
        Ok(Self {
            member_name,
            account,
        })
    }

    fn current(&self, conn: &Connection) -> Result<(), Stop> {
        let current: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_telegram_accounts a JOIN users u ON u.id=a.user_id
             WHERE a.user_id=? AND a.attempt=? AND a.phase='claimed' AND a.expires_at>? AND u.username=? AND u.disabled=0)",
            params![self.account.user_id, self.account.attempt.to_string(), crate::now(), self.member_name], |r| r.get(0),
        ).map_err(|_| Stop::Failed)?;
        if current { Ok(()) } else { Err(Stop::Expired) }
    }

    fn finish(&self, conn: &Connection, phase: &str, id: Option<i64>) -> Result<(), Stop> {
        let changed = conn.execute(
            "UPDATE native_telegram_accounts SET phase=?,telegram_id=? WHERE user_id=? AND attempt=? AND phase='claimed' AND expires_at>?
             AND EXISTS(SELECT 1 FROM users WHERE id=? AND username=? AND disabled=0)",
            params![phase, id, self.account.user_id, self.account.attempt.to_string(), crate::now(), self.account.user_id, self.member_name],
        ).map_err(|_| Stop::Failed)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(Stop::Expired)
        }
    }

    async fn until_stale(&self, conn: &Connection) -> Stop {
        loop {
            if let Err(reason) = self.current(conn) {
                return reason;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
}

trait Prompt {
    async fn input(&mut self, message: &str) -> Result<String, Stop>;
}

// Keep echo off during provider round trips too. Discard partial input before
// returning the terminal to the operator's shell, including on cancellation.
struct Tty {
    file: File,
    original: Termios,
}

impl Tty {
    fn new(file: File) -> Result<Self, Box<dyn Error>> {
        if tcgetpgrp(&file)? != rustix::process::getpgrp() {
            return Err("Run enrollment in the foreground on the host terminal".into());
        }
        let original = tcgetattr(&file)?;
        let mut hidden = original.clone();
        hidden.local_modes.insert(LocalModes::ICANON);
        hidden
            .local_modes
            .remove(LocalModes::ECHO | LocalModes::ECHONL);
        tcsetattr(&file, OptionalActions::Flush, &hidden)?;
        Ok(Self { file, original })
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        let _ = tcsetattr(&self.file, OptionalActions::Flush, &self.original);
        let _ = writeln!(self.file);
    }
}

impl Prompt for Tty {
    async fn input(&mut self, message: &str) -> Result<String, Stop> {
        if tcgetpgrp(&self.file).map_err(|_| Stop::Failed)? != rustix::process::getpgrp() {
            return Err(Stop::Failed);
        }
        let file = &mut self.file;
        rustix::termios::tcflush(&*file, rustix::termios::QueueSelector::IFlush)
            .map_err(|_| Stop::Failed)?;
        write!(file, "\n{message} ").map_err(|_| Stop::Failed)?;
        let mut buffer = [0; 4096];
        loop {
            match file.read(&mut buffer) {
                Ok(n) => {
                    let line = buffer[..n].strip_suffix(b"\n").ok_or(Stop::Declined)?;
                    if line.is_empty() {
                        return Err(Stop::Declined);
                    }
                    return std::str::from_utf8(line)
                        .map(str::to_owned)
                        .map_err(|_| Stop::Failed);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                Err(_) => return Err(Stop::Failed),
            }
        }
    }
}

async fn authorize(
    conn: &Connection,
    claim: &Claim,
    runtime: &NativeRuntime,
    prompt: &mut impl Prompt,
) -> Result<i64, Stop> {
    let member = format!(
        "{} (#{})",
        claim.member_name.escape_debug(),
        claim.account.user_id
    );
    if prompt
        .input(&format!(
            "核对发起申请的 PolyView 成员 {member}。同意继续请输入 YES（输入隐藏）："
        ))
        .await?
        != "YES"
    {
        return Err(Stop::Declined);
    }
    claim.current(conn)?;
    let client = runtime
        .client(claim.account.clone())
        .await
        .map_err(|_| Stop::Failed)?;
    let mut auth = client.auth();
    let mut previous = AuthState::Starting;
    loop {
        claim.current(conn)?;
        let stage = auth.borrow_and_update().clone();
        let question = match (&previous, &stage) {
            (AuthState::Starting, AuthState::Phone) => "Telegram 手机号（含国家区号，输入隐藏）：",
            (AuthState::Phone, AuthState::Code) => "Telegram 验证码（输入隐藏）：",
            (AuthState::Code, AuthState::Password) => "Telegram 两步验证密码（输入隐藏）：",
            (AuthState::Code | AuthState::Password, AuthState::Ready) => {
                let me = client
                    .request(json!({"@type":"getMe"}))
                    .await
                    .map_err(|_| Stop::Failed)?;
                let id = me["id"]
                    .as_i64()
                    .filter(|id| *id > 0 && me["@type"] == "user")
                    .ok_or(Stop::Failed)?;
                claim.current(conn)?;
                if prompt.input(&format!("确认将 Telegram 账户 #{id} 绑定至 PolyView 成员 {member}？同意请输入 YES（输入隐藏）：")).await? != "YES" {
                    return Err(Stop::Declined);
                }
                claim.current(conn)?;
                if *auth.borrow() != AuthState::Ready {
                    return Err(Stop::Failed);
                }
                return Ok(id);
            }
            (_, AuthState::Unsupported) => return Err(Stop::Official),
            _ => return Err(Stop::Failed),
        };
        let input = prompt.input(question).await?;
        claim.current(conn)?;
        client
            .submit_factor(&stage, input)
            .await
            .map_err(|_| Stop::Failed)?;
        drop(
            auth.wait_for(|state| *state != stage)
                .await
                .map_err(|_| Stop::Failed)?,
        );
        previous = stage;
    }
}

async fn enroll(
    conn: &Connection,
    claim: Claim,
    runtime: &NativeRuntime,
    prompt: &mut impl Prompt,
    interrupt: impl Future<Output = ()>,
) -> Result<(), Stop> {
    let result = tokio::select! {
        biased;
        _ = interrupt => Err(Stop::Declined),
        reason = claim.until_stale(conn) => Err(reason),
        _ = tokio::time::sleep(Duration::from_secs(LIFETIME as u64)) => Err(Stop::Expired),
        result = authorize(conn, &claim, runtime, prompt) => result,
    };
    let account = &claim.account;
    let result = match result {
        Ok(id) => {
            let published = runtime
                .release_session(account)
                .await
                .map_err(|_| Stop::Failed)
                .and_then(|()| claim.finish(conn, "authorized", Some(id)));
            match published {
                Ok(()) => return Ok(()),
                Err(reason) => {
                    // Cancellation or a close error can win during handoff. Reopen
                    // only this never-published attempt for explicit logout.
                    let _ = runtime.client(account.clone()).await;
                    reason
                }
            }
        }
        Err(reason) => reason,
    };
    let _ = claim.finish(conn, result.phase(), None);
    runtime.discard_session(account).await;
    Err(result)
}

pub async fn terminal_login(
    mut args: impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--member-id")) {
        return Err(USAGE.into());
    }
    let member_id = args
        .next()
        .and_then(|s| s.to_str()?.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .ok_or(USAGE)?;
    if args.next().is_some() {
        return Err(USAGE.into());
    }
    let fd = open(
        "/dev/tty",
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| "Open an interactive host terminal (Docker: exec -it)")?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut quit = signal(SignalKind::quit())?;
    let mut suspend = signal(SignalKind::from_raw(rustix::process::Signal::TSTP.as_raw()))?;
    let data_dir = std::env::var_os("POLYVIEW_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "data".into());
    let mut conn = Connection::open_with_flags(
        data_dir.join("polyview.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    conn.busy_timeout(Duration::from_secs(2))?;
    let mut tty = Tty::new(File::from(fd))?;
    let claim = Claim::take(&mut conn, member_id).map_err(|reason| match reason {
        Stop::Expired => "No unclaimed, unexpired terminal request for this member",
        _ => "Unable to claim the member's request in this PolyView data directory",
    })?;
    enroll(&conn, claim, &NativeRuntime::default(), &mut tty, async {
        tokio::select! {
            _ = interrupt.recv() => (),
            _ = terminate.recv() => (),
            _ = hangup.recv() => (),
            _ = quit.recv() => (),
            _ = suspend.recv() => (),
        }
    })
    .await
    .map_err(|reason| match reason {
        Stop::Expired => "The request expired or was canceled",
        Stop::Declined => "Enrollment declined",
        Stop::Official => "Telegram requires a verification step outside this terminal flow",
        Stop::Failed => {
            "Enrollment failed. Check the host TDLib installation and request a new attempt"
        }
    })?;
    writeln!(tty.file, "\nTelegram 账户绑定成功。")?;
    Ok(())
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;

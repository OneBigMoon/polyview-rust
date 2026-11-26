use super::*;
use crate::telegram::login::{NativeBackup, restore_config, verified_account};
use serde_json::Value;
use std::{collections::VecDeque, future::pending, path::Path, sync::mpsc};

const MEMBER: i64 = 7;
const TELEGRAM_ID: i64 = 81234567;

fn database(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    crate::init_schema(&conn).unwrap();
    conn.execute_batch("INSERT INTO users(id,username,password_hash,role,created_at) VALUES(7,'member','unused','user',0),(8,'another','unused','user',0)").unwrap();
    restore_config(
        &conn,
        NativeBackup {
            api_id: 1234,
            api_hash: "a".repeat(32),
        },
    )
    .unwrap();
    conn.execute("INSERT INTO native_telegram_accounts(user_id,account_key,attempt,phase,expires_at) VALUES(?,?,?,'terminal',?)",
        params![MEMBER, Uuid::new_v4().to_string(), Uuid::new_v4().to_string(), crate::now() + LIFETIME]).unwrap();
    conn
}

struct Answers<'a> {
    answers: VecDeque<&'a str>,
    questions: Vec<String>,
}

impl<'a> Answers<'a> {
    fn new(answers: &[&'a str]) -> Self {
        Self {
            answers: answers.iter().copied().collect(),
            questions: Vec::new(),
        }
    }
}

impl Prompt for Answers<'_> {
    async fn input(&mut self, message: &str) -> Result<String, Stop> {
        self.questions.push(message.to_owned());
        Ok(self
            .answers
            .pop_front()
            .expect("unexpected prompt")
            .to_owned())
    }
}

fn change(stage: &str) -> Value {
    json!({"@type":"updateAuthorizationState","authorization_state":{"@type":stage}})
}

fn provider(
    root: &Path,
    initial: &'static str,
    password: bool,
) -> (NativeRuntime, mpsc::Receiver<String>) {
    let (sent, received) = mpsc::channel();
    let marker = std::sync::Mutex::new(None::<PathBuf>);
    let runtime = NativeRuntime::with_transport(root.into(), move |member, request| {
        assert_eq!(member, MEMBER);
        let kind = request["@type"].as_str().unwrap();
        sent.send(kind.to_owned()).unwrap();
        let stage = match kind {
            "getAuthorizationState" => return vec![json!({"@type":initial})],
            "setTdlibParameters" => {
                let path = Path::new(request["database_directory"].as_str().unwrap())
                    .join("fixture-session");
                let ready = path.is_file();
                *marker.lock().unwrap() = Some(path);
                if ready {
                    "authorizationStateReady"
                } else {
                    "authorizationStateWaitPhoneNumber"
                }
            }
            "setAuthenticationPhoneNumber" => {
                assert_eq!(request["phone_number"], "+0000000");
                "authorizationStateWaitCode"
            }
            "checkAuthenticationCode" => {
                assert_eq!(request["code"], "0123");
                if password {
                    "authorizationStateWaitPassword"
                } else {
                    "authorizationStateReady"
                }
            }
            "checkAuthenticationPassword" => {
                assert_eq!(request["password"], " fixture ");
                "authorizationStateReady"
            }
            "getMe" => {
                if let Some(file) = marker.lock().unwrap().as_ref() {
                    std::fs::write(file, b"fixture").unwrap();
                }
                return vec![json!({"@type":"user","id":TELEGRAM_ID})];
            }
            "getChats" => return vec![json!({"@type":"chats","chat_ids":[42]})],
            "logOut" | "close" | "destroy" => {
                return vec![json!({"@type":"ok"}), change("authorizationStateClosed")];
            }
            _ => panic!("unexpected TDLib method"),
        };
        vec![change(stage), json!({"@type":"ok"})]
    });
    (runtime, received)
}

#[test]
fn only_one_operator_can_claim_the_requested_member_before_expiry() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("polyview.sqlite3");
    let mut conn = database(&path);
    let mut second = Connection::open(path).unwrap();
    assert!(matches!(Claim::take(&mut second, 8), Err(Stop::Expired)));
    conn.execute("UPDATE users SET disabled=1 WHERE id=7", [])
        .unwrap();
    assert!(matches!(
        Claim::take(&mut second, MEMBER),
        Err(Stop::Expired)
    ));
    conn.execute("UPDATE users SET disabled=0 WHERE id=7", [])
        .unwrap();
    let claim = Claim::take(&mut conn, MEMBER).unwrap();
    assert!(matches!(
        Claim::take(&mut second, MEMBER),
        Err(Stop::Expired)
    ));
    assert!(verified_account(&second, MEMBER).unwrap().is_none());
    second
        .execute(
            "UPDATE native_telegram_accounts SET expires_at=0 WHERE user_id=7",
            [],
        )
        .unwrap();
    assert_eq!(claim.current(&conn), Err(Stop::Expired));
    assert_eq!(
        claim.finish(&conn, "authorized", Some(TELEGRAM_ID)),
        Err(Stop::Expired)
    );
    assert!(verified_account(&second, MEMBER).unwrap().is_none());
}

#[tokio::test]
async fn terminal_confirms_both_identities_and_hands_off_the_native_session() {
    for password in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let mut conn = database(&root.path().join("polyview.sqlite3"));
        let claim = Claim::take(&mut conn, MEMBER).unwrap();
        let scoped = claim.account.clone();
        let (runtime, received) = provider(
            root.path(),
            "authorizationStateWaitTdlibParameters",
            password,
        );
        let mut answers = if password {
            Answers::new(&["YES", "+0000000", "0123", " fixture ", "YES"])
        } else {
            Answers::new(&["YES", "+0000000", "0123", "YES"])
        };
        enroll(&conn, claim, &runtime, &mut answers, pending())
            .await
            .unwrap();
        assert!(answers.answers.is_empty());
        assert!(answers.questions.first().unwrap().contains("member (#7)"));
        let bind = answers.questions.last().unwrap();
        assert!(bind.contains("member (#7)") && bind.contains("#81234567"));
        assert_eq!(received.try_iter().last().as_deref(), Some("close"));
        let account = verified_account(&conn, MEMBER).unwrap().unwrap();
        assert_eq!(
            (account.account_key, account.attempt),
            (scoped.account_key, scoped.attempt)
        );
        assert!(verified_account(&conn, 8).unwrap().is_none());
        let path = root
            .path()
            .join("telegram")
            .join(account.account_key.simple().to_string())
            .join(account.attempt.simple().to_string());
        assert!(path.is_dir());
        let stored_id: i64 = conn
            .query_row(
                "SELECT telegram_id FROM native_telegram_accounts WHERE user_id=7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored_id, TELEGRAM_ID);
        let (server, _received) =
            provider(root.path(), "authorizationStateWaitTdlibParameters", false);
        let state = std::sync::Arc::new(crate::AppState {
            db: std::sync::Mutex::new(conn),
            epg_refresh: Default::default(),
            playback_grants: Default::default(),
            telegram_native: server,
            cloud_fixture: None,
            download_runtime: Default::default(),
            media_dir: root.path().join("media"),
            download_dir: root.path().join("downloads"),
            client: reqwest::Client::new(),
            secure_cookie: false,
        });
        let client = crate::telegram::native::authorized(&state, MEMBER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            client.request(json!({"@type":"getChats"})).await.unwrap()["chat_ids"],
            json!([42])
        );
        assert!(
            crate::telegram::native::authorized(&state, 8)
                .await
                .unwrap()
                .is_none()
        );
        state.telegram_native.close(MEMBER, account.attempt).await;
    }
}

#[tokio::test]
async fn unexpected_initial_state_never_trusts_a_preexisting_identity() {
    for (initial, expected) in [
        ("authorizationStateReady", Stop::Failed),
        ("authorizationStateWaitPassword", Stop::Failed),
        ("authorizationStateWaitEmailAddress", Stop::Official),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut conn = database(&root.path().join("polyview.sqlite3"));
        let claim = Claim::take(&mut conn, MEMBER).unwrap();
        let (runtime, methods) = provider(root.path(), initial, false);
        let mut answers = Answers::new(&["YES"]);
        assert_eq!(
            enroll(&conn, claim, &runtime, &mut answers, pending()).await,
            Err(expected)
        );
        assert_eq!(
            methods.try_iter().collect::<Vec<_>>(),
            ["getAuthorizationState", "logOut"]
        );
        assert!(verified_account(&conn, MEMBER).unwrap().is_none());
    }
}

#[tokio::test]
async fn refusing_either_confirmation_never_publishes_a_binding() {
    for final_refusal in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let mut conn = database(&root.path().join("polyview.sqlite3"));
        let claim = Claim::take(&mut conn, MEMBER).unwrap();
        let (runtime, methods) = provider(root.path(), "authorizationStateWaitPhoneNumber", false);
        let mut answers = if final_refusal {
            Answers::new(&["YES", "+0000000", "0123", "no"])
        } else {
            Answers::new(&["no"])
        };
        assert_eq!(
            enroll(&conn, claim, &runtime, &mut answers, pending()).await,
            Err(Stop::Declined)
        );
        if final_refusal {
            assert_eq!(methods.try_iter().last().as_deref(), Some("logOut"));
        } else {
            assert!(methods.try_recv().is_err());
        }
        assert!(verified_account(&conn, MEMBER).unwrap().is_none());
    }
}

#[tokio::test]
async fn canceled_or_replaced_request_never_submits_a_pending_code() {
    struct CancelAtCode<'a>(&'a Connection);
    impl Prompt for CancelAtCode<'_> {
        async fn input(&mut self, message: &str) -> Result<String, Stop> {
            if message.contains("验证码") {
                self.0.execute("UPDATE native_telegram_accounts SET phase='terminal',attempt=? WHERE user_id=7", [Uuid::new_v4().to_string()]).unwrap();
                return pending().await;
            }
            Ok(if message.contains("手机号") {
                "+0000000"
            } else {
                "YES"
            }
            .into())
        }
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("polyview.sqlite3");
    let mut conn = database(&path);
    let second = Connection::open(path).unwrap();
    let claim = Claim::take(&mut conn, MEMBER).unwrap();
    let (runtime, methods) = provider(root.path(), "authorizationStateWaitPhoneNumber", false);
    assert_eq!(
        enroll(
            &conn,
            claim,
            &runtime,
            &mut CancelAtCode(&second),
            pending()
        )
        .await,
        Err(Stop::Expired)
    );
    assert_eq!(
        methods.try_iter().collect::<Vec<_>>(),
        [
            "getAuthorizationState",
            "setAuthenticationPhoneNumber",
            "logOut"
        ]
    );
    assert!(Claim::take(&mut conn, MEMBER).is_ok());
}

#[tokio::test]
async fn rejected_code_stops_without_password_or_binding() {
    let root = tempfile::tempdir().unwrap();
    let mut conn = database(&root.path().join("polyview.sqlite3"));
    let claim = Claim::take(&mut conn, MEMBER).unwrap();
    let (sent, methods) = mpsc::channel();
    let runtime = NativeRuntime::with_transport(root.path().into(), move |_, req| {
        let kind = req["@type"].as_str().unwrap();
        sent.send(kind.to_owned()).unwrap();
        match kind {
            "getAuthorizationState" => vec![json!({"@type":"authorizationStateWaitPhoneNumber"})],
            "setAuthenticationPhoneNumber" => {
                vec![json!({"@type":"ok"}), change("authorizationStateWaitCode")]
            }
            "checkAuthenticationCode" => vec![
                json!({"@type":"error","code":400,"message":"synthetic private provider text"}),
            ],
            "logOut" => vec![json!({"@type":"ok"}), change("authorizationStateClosed")],
            _ => panic!("unexpected TDLib method"),
        }
    });
    let mut answers = Answers::new(&["YES", "+0000000", "bad"]);
    assert_eq!(
        enroll(&conn, claim, &runtime, &mut answers, pending()).await,
        Err(Stop::Failed)
    );
    assert_eq!(
        methods.try_iter().collect::<Vec<_>>(),
        [
            "getAuthorizationState",
            "setAuthenticationPhoneNumber",
            "checkAuthenticationCode",
            "logOut"
        ]
    );
    assert!(
        answers
            .questions
            .iter()
            .all(|question| !question.contains("private"))
    );
    assert!(verified_account(&conn, MEMBER).unwrap().is_none());
}

#[tokio::test]
async fn web_cancellation_during_close_wins_over_final_local_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("polyview.sqlite3");
    let mut conn = database(&path);
    let claim = Claim::take(&mut conn, MEMBER).unwrap();
    let attempt_path = root
        .path()
        .join("telegram")
        .join(claim.account.account_key.simple().to_string())
        .join(claim.account.attempt.simple().to_string());
    let (sent, methods) = mpsc::channel();
    let runtime = NativeRuntime::with_transport(root.path().into(), move |_, req| {
        let kind = req["@type"].as_str().unwrap();
        sent.send(kind.to_owned()).unwrap();
        match kind {
            "getAuthorizationState" => vec![json!({"@type":"authorizationStateWaitPhoneNumber"})],
            "setAuthenticationPhoneNumber" => {
                vec![json!({"@type":"ok"}), change("authorizationStateWaitCode")]
            }
            "checkAuthenticationCode" => {
                vec![json!({"@type":"ok"}), change("authorizationStateReady")]
            }
            "getMe" => vec![json!({"@type":"user","id":TELEGRAM_ID})],
            "close" => {
                Connection::open(&path)
                    .unwrap()
                    .execute(
                        "UPDATE native_telegram_accounts SET phase='idle' WHERE user_id=7",
                        [],
                    )
                    .unwrap();
                vec![json!({"@type":"ok"}), change("authorizationStateClosed")]
            }
            "logOut" => vec![json!({"@type":"ok"}), change("authorizationStateClosed")],
            _ => panic!("unexpected TDLib method"),
        }
    });
    let mut answers = Answers::new(&["YES", "+0000000", "0123", "YES"]);
    assert_eq!(
        enroll(&conn, claim, &runtime, &mut answers, pending()).await,
        Err(Stop::Expired)
    );
    assert_eq!(methods.try_iter().last().as_deref(), Some("logOut"));
    let phase: String = conn
        .query_row(
            "SELECT phase FROM native_telegram_accounts WHERE user_id=7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(phase, "idle");
    assert!(verified_account(&conn, MEMBER).unwrap().is_none());
    assert!(!attempt_path.exists());
}

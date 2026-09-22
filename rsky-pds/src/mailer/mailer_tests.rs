use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn mailgun_messages_keep_template_and_moderation_fields_at_the_sdk_boundary() {
    let _env = ENV_LOCK.lock().await;
    let settings = [
        "PDS_EMAIL_SMTP_URL",
        "PDS_MAILGUN_API_KEY",
        "PDS_MAILGUN_DOMAIN",
        "PDS_EMAIL_FROM_NAME",
        "PDS_EMAIL_FROM_ADDRESS",
        "PDS_MODERATION_EMAIL_FROM_NAME",
        "PDS_MODERATION_EMAIL_FROM_ADDRESS",
    ];
    let previous: Vec<_> = settings
        .iter()
        .map(|key| (*key, std::env::var_os(key)))
        .collect();
    std::env::remove_var("PDS_EMAIL_SMTP_URL");
    std::env::set_var("PDS_MAILGUN_API_KEY", "test-key");
    std::env::set_var("PDS_MAILGUN_DOMAIN", "mail.example.test");
    std::env::set_var("PDS_EMAIL_FROM_NAME", "Accounts");
    std::env::set_var("PDS_EMAIL_FROM_ADDRESS", "accounts@example.test");
    std::env::set_var("PDS_MODERATION_EMAIL_FROM_NAME", "Moderation");
    std::env::set_var(
        "PDS_MODERATION_EMAIL_FROM_ADDRESS",
        "moderation@example.test",
    );
    let mailer = Mailer::from_env().unwrap();
    let mut vars = HashMap::new();
    vars.insert("token".to_owned(), "short-lived-token".to_owned());
    mailer
        .send_template_with(
            MailOpts {
                to: "recipient@example.test".to_owned(),
                subject: "Confirm your account".to_owned(),
                template: "confirm email".to_owned(),
                template_vars: vars.clone(),
            },
            move |client, sender| {
                Box::pin(async move {
                    assert_eq!(client.api_key, "test-key");
                    assert_eq!(client.domain, "mail.example.test");
                    assert_eq!(
                        client.message.to,
                        vec![EmailAddress::address("recipient@example.test")]
                    );
                    assert_eq!(client.message.subject, "Confirm your account");
                    assert_eq!(client.message.template, "confirm email");
                    assert_eq!(client.message.template_vars, vars);
                    assert_eq!(sender.to_string(), "Accounts <accounts@example.test>");
                    Ok(())
                })
            },
        )
        .await
        .unwrap();

    let failure = mailer
        .send_template_with(
            MailOpts {
                to: "recipient@example.test".to_owned(),
                subject: "Confirm your account".to_owned(),
                template: "confirm email".to_owned(),
                template_vars: HashMap::new(),
            },
            |_, _| Box::pin(async { Err(anyhow::anyhow!("provider refused message")) }),
        )
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("provider refused message"));

    mailer
        .send_html_with(
            "reported@example.test",
            "Moderation notice",
            "<p>Review complete</p>",
            None,
            |client, sender| {
                Box::pin(async move {
                    assert_eq!(client.api_key, "test-key");
                    assert_eq!(client.domain, "mail.example.test");
                    assert_eq!(
                        client.message.to,
                        vec![EmailAddress::address("reported@example.test")]
                    );
                    assert_eq!(client.message.subject, "Moderation notice");
                    assert_eq!(client.message.html, "<p>Review complete</p>");
                    assert_eq!(sender.to_string(), "Moderation <moderation@example.test>");
                    Ok(())
                })
            },
        )
        .await
        .unwrap();

    let failure = mailer
        .send_html_with(
            "reported@example.test",
            "Moderation notice",
            "<p>Review complete</p>",
            None,
            |_, _| Box::pin(async { Err(anyhow::anyhow!("moderation provider refused")) }),
        )
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("moderation provider refused"));

    for (key, value) in previous {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

/// Tests that change mail settings in the environment take this lock.
pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One SMTP conversation captured by a throwaway server.
pub(crate) struct Captured {
    pub commands: Vec<String>,
    pub data: String,
}

/// A minimal SMTP server that accepts one message and records it.
pub(crate) fn smtp_server(with_auth: bool) -> (u16, Arc<Mutex<Option<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured: Arc<Mutex<Option<Captured>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&captured);
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut commands = Vec::new();
        let mut data = String::new();
        writer.write_all(b"220 test ESMTP\r\n").unwrap();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let trimmed = line.trim_end().to_owned();
            commands.push(trimmed.clone());
            let upper = trimmed.to_ascii_uppercase();
            let reply: &str = if upper.starts_with("EHLO") {
                if with_auth {
                    "250-test\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n"
                } else {
                    "250-test\r\n250 8BITMIME\r\n"
                }
            } else if upper.starts_with("AUTH") {
                "235 ok\r\n"
            } else if upper.starts_with("MAIL") || upper.starts_with("RCPT") {
                "250 ok\r\n"
            } else if upper.starts_with("DATA") {
                writer.write_all(b"354 go\r\n").unwrap();
                loop {
                    let mut body = String::new();
                    if reader.read_line(&mut body).unwrap_or(0) == 0 || body == ".\r\n" {
                        break;
                    }
                    data.push_str(&body);
                }
                *sink.lock().unwrap() = Some(Captured {
                    commands: commands.clone(),
                    data: data.clone(),
                });
                "250 queued\r\n"
            } else if upper.starts_with("QUIT") {
                writer.write_all(b"221 bye\r\n").unwrap();
                break;
            } else {
                "250 ok\r\n"
            };
            writer.write_all(reply.as_bytes()).unwrap();
        }
        drop((commands, data));
    });
    (port, captured)
}

fn wait_for(captured: &Arc<Mutex<Option<Captured>>>) -> Captured {
    for _ in 0..200 {
        if let Some(captured) = captured.lock().unwrap().take() {
            return captured;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("the SMTP server captured nothing");
}

#[test]
fn templates_render_with_escaped_values() {
    for (name, html) in TEMPLATES {
        assert!(html.contains("{{token}}"), "{name} takes a token");
        assert!(template(name).is_some());
    }
    assert!(template("nope").is_none());
    let mut vars = HashMap::new();
    vars.insert("identifier".to_string(), "alice.<b>test</b>".to_string());
    vars.insert("token".to_string(), "ABCDE-FGHIJ".to_string());
    let html = render(template("reset password").unwrap(), &vars);
    assert!(html.contains("@alice.&lt;b&gt;test&lt;/b&gt;"));
    assert!(html.contains("ABCDE-FGHIJ"));
    assert!(!html.contains("{{"));
    let text = plain_text(&html);
    assert!(text.contains("ABCDE-FGHIJ"));
    assert!(
        text.contains("@alice.<b>test</b>"),
        "entities decode back in the text part"
    );
    assert!(!text.contains("<td") && !text.contains("<table"), "{text}");
    assert_eq!(escape("a&b\"'"), "a&amp;b&quot;&#x27;");
}

#[tokio::test]
async fn smtp_urls_follow_the_reference_forms() {
    assert!(smtp_transport("smtps://user:p%40ss@mail.example.test:465").is_ok());
    assert!(smtp_transport("smtp://mail.example.test:587").is_ok());
    assert!(smtp_transport("smtp://127.0.0.1:2525?ignoreTLS=true").is_ok());
    assert!(smtp_transport("smtp://127.0.0.1:2525").is_ok());
    assert!(smtp_transport("http://mail.example.test").is_err());
    assert!(smtp_transport("not a url").is_err());
    assert!(smtp_transport("smtp:///nohost").is_err());
    assert!(Mailer::smtp("smtp://127.0.0.1:2525?ignoreTLS=true", "not a mailbox").is_err());
    assert!(Mailer::smtp(
        "smtp://127.0.0.1:2525?ignoreTLS=true",
        "Blacksky <noreply@example.test>"
    )
    .is_ok());
}

#[tokio::test]
async fn the_transport_is_chosen_from_the_environment() {
    let _env = ENV_LOCK.lock().await;
    std::env::remove_var("PDS_EMAIL_SMTP_URL");
    std::env::set_var("PDS_MAILGUN_API_KEY", "");
    assert!(matches!(
        Mailer::from_env().unwrap().transport,
        Transport::Log
    ));
    std::env::set_var("PDS_MAILGUN_API_KEY", "key");
    assert!(matches!(
        Mailer::from_env().unwrap().transport,
        Transport::Mailgun
    ));
    std::env::set_var("PDS_MAILGUN_API_KEY", "");
    std::env::set_var("PDS_EMAIL_SMTP_URL", "smtp://127.0.0.1:1?ignoreTLS=true");
    std::env::remove_var("PDS_EMAIL_FROM_ADDRESS");
    assert!(Mailer::from_env().is_err());
    std::env::set_var("PDS_EMAIL_FROM_ADDRESS", "noreply@example.test");
    assert!(Mailer::from_env().unwrap().is_smtp());
    std::env::remove_var("PDS_EMAIL_SMTP_URL");
    std::env::remove_var("PDS_EMAIL_FROM_ADDRESS");
    // the logging mailer completes every flow
    let logging = Mailer::logging();
    assert!(!logging.is_smtp());
    let mut vars = HashMap::new();
    vars.insert("token".to_string(), "T".to_string());
    logging
        .send_template(MailOpts {
            to: "a@example.test".into(),
            subject: "s".into(),
            template: "confirm email".into(),
            template_vars: vars,
        })
        .await
        .unwrap();
    logging
        .send_html("a@example.test", "s", "<p>x</p>", None)
        .await
        .unwrap();
    // the process-wide mailer carries every flow's wrapper
    let token = TokenParam {
        token: "T".to_string(),
    };
    send_reset_password(
        "a@example.test".into(),
        IdentifierAndTokenParams {
            identifier: "a.test".into(),
            token: "T".into(),
        },
    )
    .await
    .unwrap();
    send_account_delete("a@example.test".into(), token.clone())
        .await
        .unwrap();
    send_confirm_email("a@example.test".into(), token.clone())
        .await
        .unwrap();
    send_update_email("a@example.test".into(), token.clone())
        .await
        .unwrap();
    send_plc_operation("a@example.test".into(), token)
        .await
        .unwrap();
}

#[tokio::test]
async fn messages_reach_an_smtp_server_with_the_rendered_template() {
    let (port, captured) = smtp_server(true);
    let mailer = Mailer::smtp(
        &format!("smtp://user:secret@127.0.0.1:{port}?ignoreTLS=true"),
        "Blacksky <noreply@example.test>",
    )
    .unwrap();
    let mut vars = HashMap::new();
    vars.insert("identifier".to_string(), "alice.test".to_string());
    vars.insert("token".to_string(), "ABCDE-FGHIJ".to_string());
    mailer
        .send_template(MailOpts {
            to: "alice@example.test".into(),
            subject: "Password Reset Requested".into(),
            template: "reset password".into(),
            template_vars: vars,
        })
        .await
        .unwrap();
    let captured = wait_for(&captured);
    assert!(
        captured.commands.iter().any(|c| c.starts_with("AUTH")),
        "{:?}",
        captured.commands
    );
    assert!(
        captured
            .commands
            .iter()
            .any(|c| c == "MAIL FROM:<noreply@example.test>"),
        "{:?}",
        captured.commands
    );
    assert!(captured
        .commands
        .iter()
        .any(|c| c == "RCPT TO:<alice@example.test>"));
    assert!(captured.data.contains("Subject: Password Reset Requested"));
    assert!(captured.data.contains("multipart/alternative"));
    assert!(captured.data.contains("ABCDE-FGHIJ"));
    assert!(captured.data.contains("alice.test"));

    // an unknown template and an unreachable server are errors
    let mut vars = HashMap::new();
    vars.insert("token".to_string(), "T".to_string());
    let err = mailer
        .send_template(MailOpts {
            to: "alice@example.test".into(),
            subject: "s".into(),
            template: "no such template".into(),
            template_vars: vars.clone(),
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown mail template"));
    let bad_recipient = mailer
        .send_template(MailOpts {
            to: "not a mailbox".into(),
            subject: "s".into(),
            template: "confirm email".into(),
            template_vars: vars.clone(),
        })
        .await
        .unwrap_err();
    assert!(bad_recipient.to_string().contains("not a mailbox"));
    let closed = Mailer::smtp("smtp://127.0.0.1:1?ignoreTLS=true", "noreply@example.test").unwrap();
    assert!(closed
        .send_template(MailOpts {
            to: "alice@example.test".into(),
            subject: "s".into(),
            template: "confirm email".into(),
            template_vars: vars,
        })
        .await
        .is_err());
    assert!(closed
        .send_html("alice@example.test", "s", "<p>x</p>", None)
        .await
        .is_err());
    assert!(closed
        .send_html("nope", "s", "<p>x</p>", None)
        .await
        .is_err());

    // html bodies go out with a text alternative and an explicit sender;
    // a plain smtp URL stays plain when the server offers no STARTTLS
    let (port, captured) = smtp_server(false);
    let mailer = Mailer::smtp(&format!("smtp://127.0.0.1:{port}"), "noreply@example.test").unwrap();
    let from: Mailbox = "Moderation <mod@example.test>".parse().unwrap();
    mailer
        .send_html(
            "bob@example.test",
            "Notice",
            "<p>Hello <b>Bob</b></p>",
            Some(&from),
        )
        .await
        .unwrap();
    let captured = wait_for(&captured);
    assert!(!captured.commands.iter().any(|c| c.starts_with("AUTH")));
    assert!(
        captured
            .commands
            .iter()
            .any(|c| c == "MAIL FROM:<mod@example.test>"),
        "{:?}",
        captured.commands
    );
    assert!(captured.data.contains("Hello Bob"));
    assert!(captured.data.contains("<b>Bob</b>"));
}

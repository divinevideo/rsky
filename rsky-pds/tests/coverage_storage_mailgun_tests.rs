use rsky_pds::mailer::{MailOpts, Mailer};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn mailgun_sdk_failures_return_without_contacting_an_external_host() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0u8; 1024];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0, "proxy client closed before headers");
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            requests.push(String::from_utf8(request).unwrap());
            stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        }
        requests
    });

    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
        "PDS_EMAIL_SMTP_URL",
    ] {
        std::env::remove_var(key);
    }
    std::env::set_var("HTTPS_PROXY", &proxy);
    std::env::set_var("https_proxy", &proxy);
    std::env::set_var("PDS_MAILGUN_API_KEY", "local-test-key");
    std::env::set_var("PDS_MAILGUN_DOMAIN", "mail.example.test");
    std::env::set_var("PDS_EMAIL_FROM_ADDRESS", "accounts@example.test");
    std::env::set_var(
        "PDS_MODERATION_EMAIL_FROM_ADDRESS",
        "moderation@example.test",
    );
    let mailer = Mailer::from_env().unwrap();

    tokio::time::timeout(
        Duration::from_secs(10),
        mailer.send_template(MailOpts {
            to: "recipient@example.test".to_owned(),
            subject: "Confirm".to_owned(),
            template: "confirm email".to_owned(),
            template_vars: HashMap::new(),
        }),
    )
    .await
    .unwrap()
    .unwrap_err();

    tokio::time::timeout(
        Duration::from_secs(10),
        mailer.send_html("recipient@example.test", "Review", "<p>Done</p>", None),
    )
    .await
    .unwrap()
    .unwrap_err();

    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(
            headers.starts_with("CONNECT api.mailgun.net:443 HTTP/1.1\r\n"),
            "TLS tunnel was not established before sending mail"
        );
        assert!(
            !headers.to_ascii_lowercase().contains("authorization:"),
            "mail credentials were sent before the TLS tunnel"
        );
        assert!(body.is_empty(), "mail body was sent before the TLS tunnel");
    }
}

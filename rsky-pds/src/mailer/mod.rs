//! Account mail: the five reference templates, branded, sent over SMTP
//! (`PDS_EMAIL_SMTP_URL`, `PDS_EMAIL_FROM_ADDRESS`) as the reference PDS
//! does, or through Mailgun when only that is configured. Without a
//! transport the message is logged, the way the reference's development
//! transport does, so token flows still complete.

pub mod moderation;

extern crate mailgun_rs;

use anyhow::{anyhow, bail, Context, Result};
use futures::future::BoxFuture;
use lettre::message::{header, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as Mail, Tokio1Executor};
use mailgun_rs::{EmailAddress, Mailgun, MailgunRegion, Message};
use std::collections::HashMap;
use std::env;
use std::sync::OnceLock;

pub struct MailOpts {
    pub to: String,
    pub subject: String,
    pub template: String,
    pub template_vars: HashMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IdentifierAndTokenParams {
    pub identifier: String,
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TokenParam {
    pub token: String,
}

/// The branded templates the reference image sends, with `{{name}}`
/// placeholders for the values each flow supplies.
pub const TEMPLATES: [(&str, &str); 5] = [
    (
        "reset password",
        include_str!("templates/reset-password.html"),
    ),
    (
        "delete account",
        include_str!("templates/delete-account.html"),
    ),
    (
        "confirm email",
        include_str!("templates/confirm-email.html"),
    ),
    ("email update", include_str!("templates/update-email.html")),
    (
        "plc operation",
        include_str!("templates/plc-operation.html"),
    ),
];

pub fn template(name: &str) -> Option<&'static str> {
    TEMPLATES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, html)| *html)
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
    out
}

/// Fills a template's placeholders with escaped values. The reference
/// templates take `handle` and `token`; the account's handle is what the
/// password-reset flow calls `identifier`.
pub fn render(html: &str, vars: &HashMap<String, String>) -> String {
    let mut out = html.to_owned();
    for (name, value) in vars {
        let value = escape(value);
        out = out.replace(&format!("{{{{{name}}}}}"), &value);
        if name == "identifier" {
            out = out.replace("{{handle}}", &value);
        }
    }
    out
}

/// A readable text alternative for the HTML body, as the reference attaches
/// through html-to-text.
pub fn plain_text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut skip_depth = 0usize;
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if !in_tag && bytes[i] == b'<' {
            in_tag = true;
            if lower[i..].starts_with("<style")
                || lower[i..].starts_with("<head")
                || lower[i..].starts_with("<title")
            {
                skip_depth += 1;
            } else if lower[i..].starts_with("</style")
                || lower[i..].starts_with("</head")
                || lower[i..].starts_with("</title")
            {
                skip_depth = skip_depth.saturating_sub(1);
            } else if lower[i..].starts_with("<br")
                || lower[i..].starts_with("<p")
                || lower[i..].starts_with("</p")
                || lower[i..].starts_with("<div")
                || lower[i..].starts_with("<tr")
                || lower[i..].starts_with("<h")
            {
                out.push('\n');
            }
        } else if in_tag {
            if bytes[i] == b'>' {
                in_tag = false;
            }
        } else if skip_depth == 0 {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'");
    let mut lines: Vec<String> = Vec::new();
    for line in decoded.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() && line.chars().any(|c| c.is_alphanumeric()) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// An SMTP transport from a `smtp://` or `smtps://` URL as the reference
/// reads it: `smtps` for implicit TLS, `smtp` for STARTTLS when the server
/// offers it and a plain session otherwise, and `smtp` with
/// `ignoreTLS=true` for a plain session only; credentials in the URL.
pub fn smtp_transport(url: &str) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let parsed = url::Url::parse(url).with_context(|| "PDS_EMAIL_SMTP_URL is not a URL")?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("SMTP URL has no host"))?
        .to_owned();
    let plain = parsed.query_pairs().any(|(key, value)| {
        matches!(
            (key.as_ref(), value.as_ref()),
            ("ignoreTLS", "true") | ("secure", "false") | ("tls", "false")
        )
    });
    let mut builder = match (parsed.scheme(), plain) {
        ("smtps", _) => {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&host)?.port(parsed.port().unwrap_or(465))
        }
        ("smtp", true) => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host)
            .port(parsed.port().unwrap_or(25)),
        ("smtp", false) => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host)
            .port(parsed.port().unwrap_or(587))
            .tls(Tls::Opportunistic(TlsParameters::new(host.clone())?)),
        (other, _) => bail!("unsupported SMTP scheme {other}"),
    };
    if !parsed.username().is_empty() {
        let user = urlencoding::decode(parsed.username())?.into_owned();
        let pass = urlencoding::decode(parsed.password().unwrap_or_default())?.into_owned();
        builder = builder.credentials(Credentials::new(user, pass));
    }
    Ok(builder.build())
}

/// How account mail leaves this process.
pub enum Transport {
    Smtp {
        transport: AsyncSmtpTransport<Tokio1Executor>,
        from: Mailbox,
    },
    Mailgun,
    Log,
}

pub struct Mailer {
    transport: Transport,
}

impl Mailer {
    pub(super) fn sdk_mailgun_send(
        client: Mailgun,
        sender: EmailAddress,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            client
                .async_send(MailgunRegion::US, &sender)
                .await
                .map(|_| ())
                .map_err(Into::into)
        })
    }

    /// SMTP when `PDS_EMAIL_SMTP_URL` is set, Mailgun when only its key is,
    /// the log otherwise.
    pub fn from_env() -> Result<Self> {
        if let Some(url) = env::var("PDS_EMAIL_SMTP_URL")
            .ok()
            .filter(|url| !url.is_empty())
        {
            let from = env::var("PDS_EMAIL_FROM_ADDRESS")
                .ok()
                .filter(|from| !from.is_empty())
                .ok_or_else(|| {
                    anyhow!("PDS_EMAIL_FROM_ADDRESS is required with PDS_EMAIL_SMTP_URL")
                })?;
            return Self::smtp(&url, &from);
        }
        if env::var("PDS_MAILGUN_API_KEY").is_ok_and(|key| !key.is_empty()) {
            return Ok(Mailer {
                transport: Transport::Mailgun,
            });
        }
        Ok(Mailer {
            transport: Transport::Log,
        })
    }

    pub fn smtp(url: &str, from: &str) -> Result<Self> {
        let from: Mailbox = from
            .parse()
            .map_err(|err| anyhow!("PDS_EMAIL_FROM_ADDRESS {from:?} is not a mailbox: {err}"))?;
        Ok(Mailer {
            transport: Transport::Smtp {
                transport: smtp_transport(url)?,
                from,
            },
        })
    }

    pub fn logging() -> Self {
        Mailer {
            transport: Transport::Log,
        }
    }

    pub fn is_smtp(&self) -> bool {
        matches!(self.transport, Transport::Smtp { .. })
    }

    /// Renders and sends one of the templates.
    pub async fn send_template(&self, opts: MailOpts) -> Result<()> {
        self.send_template_with(opts, Self::sdk_mailgun_send).await
    }

    async fn send_template_with<F>(&self, opts: MailOpts, send_mailgun: F) -> Result<()>
    where
        F: FnOnce(Mailgun, EmailAddress) -> BoxFuture<'static, Result<()>>,
    {
        let MailOpts {
            to,
            subject,
            template: template_name,
            template_vars,
        } = opts;
        match &self.transport {
            Transport::Log => {
                tracing::info!(
                    %to,
                    %subject,
                    template = %template_name,
                    ?template_vars,
                    "no mail transport is configured; message logged"
                );
                Ok(())
            }
            Transport::Smtp { transport, from } => {
                let html = template(&template_name)
                    .ok_or_else(|| anyhow!("unknown mail template {template_name}"))?;
                let html = render(html, &template_vars);
                let text = plain_text(&html);
                let mail = Mail::builder()
                    .from(from.clone())
                    .to(to
                        .parse()
                        .map_err(|err| anyhow!("recipient {to:?} is not a mailbox: {err}"))?)
                    .subject(subject)
                    .multipart(MultiPart::alternative_plain_html(text, html))?;
                transport.send(mail).await.context("SMTP delivery failed")?;
                Ok(())
            }
            Transport::Mailgun => {
                let recipient = EmailAddress::address(&to);
                let message = Message {
                    to: vec![recipient],
                    subject,
                    template: template_name,
                    template_vars,
                    ..Default::default()
                };
                let client = Mailgun {
                    api_key: env::var("PDS_MAILGUN_API_KEY").unwrap_or_default(),
                    domain: env::var("PDS_MAILGUN_DOMAIN").unwrap_or_default(),
                    message,
                };
                let sender = EmailAddress::name_address(
                    &env::var("PDS_EMAIL_FROM_NAME").unwrap_or_default(),
                    &env::var("PDS_EMAIL_FROM_ADDRESS").unwrap_or_default(),
                );
                send_mailgun(client, sender).await
            }
        }
    }

    /// Sends a ready HTML body, as the moderation mailer needs.
    pub async fn send_html(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        from: Option<&Mailbox>,
    ) -> Result<()> {
        self.send_html_with(to, subject, html, from, Self::sdk_mailgun_send)
            .await
    }

    async fn send_html_with<F>(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        from: Option<&Mailbox>,
        send_mailgun: F,
    ) -> Result<()>
    where
        F: FnOnce(Mailgun, EmailAddress) -> BoxFuture<'static, Result<()>>,
    {
        match &self.transport {
            Transport::Log => {
                tracing::info!(%to, %subject, "no mail transport is configured; message logged");
                Ok(())
            }
            Transport::Smtp {
                transport,
                from: default_from,
            } => {
                let mail = Mail::builder()
                    .from(from.cloned().unwrap_or_else(|| default_from.clone()))
                    .to(to
                        .parse()
                        .map_err(|err| anyhow!("recipient {to:?} is not a mailbox: {err}"))?)
                    .subject(subject)
                    .multipart(
                        MultiPart::alternative()
                            .singlepart(
                                SinglePart::builder()
                                    .header(header::ContentType::TEXT_PLAIN)
                                    .body(plain_text(html)),
                            )
                            .singlepart(
                                SinglePart::builder()
                                    .header(header::ContentType::TEXT_HTML)
                                    .body(html.to_owned()),
                            ),
                    )?;
                transport.send(mail).await.context("SMTP delivery failed")?;
                Ok(())
            }
            Transport::Mailgun => {
                moderation::send_through_mailgun(to, subject, html, send_mailgun).await
            }
        }
    }
}

static MAILER: OnceLock<Mailer> = OnceLock::new();

/// The process-wide mailer, built from the environment on first use.
pub fn mailer() -> &'static Mailer {
    MAILER.get_or_init(|| match Mailer::from_env() {
        Ok(mailer) => mailer,
        Err(error) => {
            tracing::error!(%error, "mail transport misconfigured; messages will be logged");
            Mailer::logging()
        }
    })
}

pub async fn send_template(opts: MailOpts) -> Result<()> {
    mailer().send_template(opts).await
}

pub async fn send_reset_password(to: String, params: IdentifierAndTokenParams) -> Result<()> {
    let mut template_vars = HashMap::new();
    template_vars.insert("identifier".to_string(), params.identifier);
    template_vars.insert("token".to_string(), params.token);
    send_template(MailOpts {
        to,
        subject: "Password Reset Requested".to_string(),
        template: "reset password".to_string(),
        template_vars,
    })
    .await
}

pub async fn send_account_delete(to: String, params: TokenParam) -> Result<()> {
    let mut template_vars = HashMap::new();
    template_vars.insert("token".to_string(), params.token);
    send_template(MailOpts {
        to,
        subject: "Account Deletion Requested".to_string(),
        template: "delete account".to_string(),
        template_vars,
    })
    .await
}

pub async fn send_confirm_email(to: String, params: TokenParam) -> Result<()> {
    let mut template_vars = HashMap::new();
    template_vars.insert("token".to_string(), params.token);
    send_template(MailOpts {
        to,
        subject: "Email Confirmation".to_string(),
        template: "confirm email".to_string(),
        template_vars,
    })
    .await
}

pub async fn send_update_email(to: String, params: TokenParam) -> Result<()> {
    let mut template_vars = HashMap::new();
    template_vars.insert("token".to_string(), params.token);
    send_template(MailOpts {
        to,
        subject: "Email Update Requested".to_string(),
        template: "email update".to_string(),
        template_vars,
    })
    .await
}

pub async fn send_plc_operation(to: String, params: TokenParam) -> Result<()> {
    let mut template_vars = HashMap::new();
    template_vars.insert("token".to_string(), params.token);
    send_template(MailOpts {
        to,
        subject: "PLC Update Operation Requested".to_string(),
        template: "plc operation".to_string(),
        template_vars,
    })
    .await
}

#[cfg(test)]
#[path = "mailer_tests.rs"]
pub(crate) mod tests;

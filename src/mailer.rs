use lettre::message::header::ContentType;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Message, SmtpTransport, Transport};

#[derive(Clone)]
pub struct EmailConfig {
    pub enabled: bool,
    pub to: Vec<String>,
    pub username: String,
    pub password: String,
    pub host: String,
    pub smtp_port: u16,
    pub from_address: String,
    pub display_name: String,
    pub use_ssl: bool,
}

pub fn send(cfg: &EmailConfig, subject: &str, body: &str) -> Result<(), String> {
    let from_str = if cfg.display_name.is_empty() {
        cfg.from_address.clone()
    } else {
        format!("{} <{}>", cfg.display_name, cfg.from_address)
    };
    let from: Mailbox = from_str.parse().map_err(|e| format!("bad from address: {e}"))?;

    let mut builder = Message::builder().from(from).subject(subject);
    for to in &cfg.to {
        let mbox: Mailbox = to.parse().map_err(|e| format!("bad to address '{to}': {e}"))?;
        builder = builder.to(mbox);
    }
    let email = builder
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|e| format!("build message: {e}"))?;

    let creds = Credentials::new(cfg.username.clone(), cfg.password.clone());
    let transport = if cfg.use_ssl {
        let tls = TlsParameters::new(cfg.host.clone()).map_err(|e| format!("tls params: {e}"))?;
        SmtpTransport::builder_dangerous(&cfg.host)
            .port(cfg.smtp_port)
            .tls(Tls::Wrapper(tls))
            .credentials(creds)
            .build()
    } else {
        SmtpTransport::builder_dangerous(&cfg.host)
            .port(cfg.smtp_port)
            .credentials(creds)
            .build()
    };
    transport.send(&email).map_err(|e| format!("smtp send: {e}"))?;
    Ok(())
}

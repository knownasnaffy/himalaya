use std::{
    env::temp_dir,
    fs,
    io::{stdout, Write},
    path::PathBuf,
    sync::Arc,
};

use clap::Parser;
use color_eyre::{eyre::eyre, Result};
use email::{backend::feature::BackendFeatureSource, config::Config};
use mail_parser::{HeaderValue, MimeHeaders, PartType};
use pimalaya_tui::{
    himalaya::backend::BackendBuilder,
    terminal::{cli::printer::Printer, config::TomlConfig as _},
};
use serde::Serialize;
use tracing::info;

use crate::{
    account::arg::name::AccountNameFlag, config::TomlConfig, envelope::arg::ids::EnvelopeIdArg,
    folder::arg::name::FolderNameOptionalFlag,
};

/// Export the message associated to the given envelope id.
///
/// This command allows you to export a message. A message can be
/// fully exported in one single file, or exported in multiple files
/// (one per MIME part found in the message). This is useful, for
/// example, to read a HTML message.
#[derive(Debug, Parser)]
pub struct MessageExportCommand {
    #[command(flatten)]
    pub folder: FolderNameOptionalFlag,

    #[command(flatten)]
    pub envelope: EnvelopeIdArg,

    /// Export the full raw message as one unique .eml file.
    ///
    /// The raw message represents the headers and the body as it is
    /// on the backend, unedited: not decoded nor decrypted. This is
    /// useful for debugging faulty messages, but also for
    /// saving/sending/transfering messages.
    #[arg(long, short = 'F')]
    pub full: bool,

    /// Try to open the exported message, when applicable.
    ///
    /// This argument only works with full message export, or when
    /// HTML or plain text is present in the export.
    #[arg(long, short = 'O')]
    pub open: bool,

    /// Where the message should be exported to.
    ///
    /// The destination should point to a valid directory. If `--full`
    /// is given, it can also point to a .eml file.
    #[arg(long, short, alias = "dest")]
    pub destination: Option<PathBuf>,

    #[command(flatten)]
    pub account: AccountNameFlag,
}

/// Serializable representation of an exported message
#[derive(Debug, Serialize)]
pub struct ExportedMessage {
    pub id: String,
    pub headers: Vec<MessageHeader>,
    pub text_body: Vec<String>,
    pub html_body: Vec<String>,
    pub attachments: Vec<AttachmentInfo>,
}

impl std::fmt::Display for ExportedMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Message {} exported successfully", self.id)
    }
}

#[derive(Debug, Serialize)]
pub struct MessageHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct AttachmentInfo {
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub size: usize,
}

impl MessageExportCommand {
    pub async fn execute(self, printer: &mut impl Printer, config: &TomlConfig) -> Result<()> {
        info!("executing export message command");

        let folder = &self.folder.name;
        let id = &self.envelope.id;

        let (toml_account_config, account_config) = config
            .clone()
            .into_account_configs(self.account.name.as_deref(), |c: &Config, name| {
                c.account(name).ok()
            })?;

        let account_config = Arc::new(account_config);

        let backend = BackendBuilder::new(
            Arc::new(toml_account_config),
            account_config.clone(),
            |builder| {
                builder
                    .without_features()
                    .with_get_messages(BackendFeatureSource::Context)
            },
        )
        .without_sending_backend()
        .build()
        .await?;

        let msgs = backend.get_messages(folder, &[*id]).await?;
        let msg = msgs.first().ok_or(eyre!("cannot find message {id}"))?;

        // Check if JSON output is requested via printer
        if printer.is_json() {
            return self.export_json(msg, id, printer);
        }

        if self.full {
            let bytes = msg.raw()?;

            match self.destination {
                Some(mut dest) if dest.is_dir() => {
                    dest.push(format!("{id}.eml"));
                    fs::write(&dest, bytes)?;
                    let dest = dest.display();
                    printer.out(format!("Message {id} successfully exported at {dest}!\n"))?;
                }
                Some(dest) => {
                    fs::write(&dest, bytes)?;
                    let dest = dest.display();
                    printer.out(format!("Message {id} successfully exported at {dest}!\n"))?;
                }
                None => {
                    stdout().write_all(bytes)?;
                }
            };
        } else {
            let dest = match self.destination {
                Some(dest) if dest.is_dir() => {
                    let dest = msg.download_parts(&dest)?;
                    let d = dest.display();
                    printer.out(format!("Message {id} successfully exported in {d}!\n"))?;
                    dest
                }
                Some(dest) if dest.is_file() => {
                    let dest = dest.parent().unwrap_or(&dest);
                    let dest = msg.download_parts(&dest)?;
                    let d = dest.display();
                    printer.out(format!("Message {id} successfully exported in {d}!\n"))?;
                    dest
                }
                Some(dest) => {
                    return Err(eyre!("Destination {} does not exist!", dest.display()));
                }
                None => {
                    let dest = temp_dir();
                    let dest = msg.download_parts(&dest)?;
                    let d = dest.display();
                    printer.out(format!("Message {id} successfully exported in {d}!\n"))?;
                    dest
                }
            };

            if self.open {
                let index_html = dest.join("index.html");
                if index_html.exists() {
                    return Ok(open::that(index_html)?);
                }

                let plain_txt = dest.join("plain.txt");
                if plain_txt.exists() {
                    return Ok(open::that(plain_txt)?);
                }

                printer.out("--open was passed but nothing to open, ignoring\n")?;
            }
        }

        Ok(())
    }

    fn export_json(
        &self,
        msg: &email::email::message::Message,
        id: &usize,
        printer: &mut impl Printer,
    ) -> Result<()> {
        let parsed = msg.parsed()?;

        // Extract headers
        let headers: Vec<MessageHeader> = parsed
            .headers()
            .iter()
            .map(|h| {
                let value = match h.value() {
                    HeaderValue::Text(t) => t.to_string(),
                    HeaderValue::TextList(list) => list.join(", "),
                    HeaderValue::Address(addr) => {
                        // Extract addresses from the Address enum
                        use mail_parser::Address;
                        match addr {
                            Address::List(addrs) => {
                                addrs.iter()
                                    .filter_map(|a| {
                                        let email = a.address.as_ref().map(|e| e.as_ref())?;
                                        if let Some(name) = &a.name {
                                            Some(format!("{} <{}>", name, email))
                                        } else {
                                            Some(email.to_string())
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            }
                            Address::Group(groups) => {
                                groups.iter()
                                    .map(|g| {
                                        let addrs = g.addresses.iter()
                                            .filter_map(|a| {
                                                let email = a.address.as_ref().map(|e| e.as_ref())?;
                                                if let Some(name) = &a.name {
                                                    Some(format!("{} <{}>", name, email))
                                                } else {
                                                    Some(email.to_string())
                                                }
                                            })
                                            .collect::<Vec<_>>()
                                            .join(", ");
                                        if let Some(name) = &g.name {
                                            format!("{}: {}", name, addrs)
                                        } else {
                                            addrs
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            }
                        }
                    }
                    HeaderValue::DateTime(dt) => dt.to_rfc3339(),
                    HeaderValue::ContentType(ct) => {
                        format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("unknown"))
                    }
                    HeaderValue::Received(rcv) => {
                        // Format Received header in a readable way
                        let mut parts = Vec::new();
                        if let Some(from) = &rcv.from {
                            parts.push(format!("from {}", from));
                        }
                        if let Some(by) = &rcv.by {
                            parts.push(format!("by {}", by));
                        }
                        if let Some(with) = &rcv.with {
                            parts.push(format!("with {:?}", with));
                        }
                        if let Some(id) = &rcv.id {
                            parts.push(format!("id {}", id));
                        }
                        parts.join(" ")
                    }
                    _ => format!("{:?}", h.value()),
                };

                MessageHeader {
                    name: h.name().to_string(),
                    value,
                }
            })
            .collect();

        // Extract text bodies
        let text_body: Vec<String> = parsed
            .text_body
            .iter()
            .filter_map(|&part_id| {
                parsed.parts.get(part_id).and_then(|part| {
                    if let PartType::Text(text) = &part.body {
                        Some(text.to_string())
                    } else {
                        None
                    }
                })
            })
            .collect();

        // Extract HTML bodies
        let html_body: Vec<String> = parsed
            .html_body
            .iter()
            .filter_map(|&part_id| {
                parsed.parts.get(part_id).and_then(|part| {
                    if let PartType::Html(html) = &part.body {
                        Some(html.to_string())
                    } else {
                        None
                    }
                })
            })
            .collect();

        // Extract attachment info
        let attachments: Vec<AttachmentInfo> = parsed
            .attachments
            .iter()
            .filter_map(|&part_id| {
                parsed.parts.get(part_id).map(|part| {
                    let size = match &part.body {
                        PartType::Binary(bin) | PartType::InlineBinary(bin) => bin.len(),
                        PartType::Text(text) => text.len(),
                        PartType::Html(html) => html.len(),
                        _ => 0,
                    };

                    // Get content type from headers
                    let content_type = part.headers.iter()
                        .find(|h| h.name().eq_ignore_ascii_case("content-type"))
                        .and_then(|h| {
                            if let HeaderValue::ContentType(ct) = h.value() {
                                Some(format!(
                                    "{}/{}",
                                    ct.ctype(),
                                    ct.subtype().unwrap_or("unknown")
                                ))
                            } else {
                                None
                            }
                        });

                    // Get filename from Content-Disposition or Content-Type
                    let filename = part.headers.iter()
                        .find(|h| h.name().eq_ignore_ascii_case("content-disposition"))
                        .and_then(|h| {
                            if let HeaderValue::ContentType(ct) = h.value() {
                                ct.attribute("filename").map(|s: &str| s.to_string())
                            } else {
                                None
                            }
                        })
                        .or_else(|| {
                            part.headers.iter()
                                .find(|h| h.name().eq_ignore_ascii_case("content-type"))
                                .and_then(|h| {
                                    if let HeaderValue::ContentType(ct) = h.value() {
                                        ct.attribute("name").map(|s: &str| s.to_string())
                                    } else {
                                        None
                                    }
                                })
                        });

                    AttachmentInfo {
                        filename,
                        content_type,
                        size,
                    }
                })
            })
            .collect();

        let exported = ExportedMessage {
            id: id.to_string(),
            headers,
            text_body,
            html_body,
            attachments,
        };

        printer.out(exported)
    }
}

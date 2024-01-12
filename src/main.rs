use std::env;
use std::path::PathBuf;

use anyhow::bail;
use clap::{Parser, Subcommand};
use clap_verbosity_flag::Verbosity;

use futures::StreamExt;
use matrix_sdk::ruma::presence::PresenceState;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, OwnedUserId};

use serde::Serialize;
use serde_json::value::RawValue;

mod client;
mod mime;
mod outputs;
mod terminal;
mod util;

use crate::client::{session, Client};

const CRATE_NAME: &str = clap::crate_name!();

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    verbose: Verbosity,

    /// Request the full state during sync
    #[arg(short, long)]
    full_state: bool,

    /// Presence value while syncing
    #[arg(short, long, default_value = "online")]
    presense: PresenceState,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Delete session store and secrets (dangerous!)
    Clean { user_id: OwnedUserId },
    /// Get information about your homeserver and login
    #[command(alias = "hs")]
    Homeserver {
        /// Really print the token
        #[arg(short, long)]
        force: bool,

        /// Include the bearer token
        #[arg(short = 't', long = "token")]
        include_token: bool,
    },
    /// Login to a homeserver and create a session store
    Login {
        user_id: OwnedUserId,

        #[arg(short, long)]
        password: Option<String>,

        #[arg(short, long, default_value = CRATE_NAME)]
        device_name: String,
    },
    /// Logout and delete all state
    Logout {},
    /// Dump messages of a room
    Messages {
        #[arg(short, long, required = true)]
        room_id: OwnedRoomId,

        /// Dump all event types
        // #[arg(short, long)]
        // all_types: bool,

        /// Only request this number of events
        #[arg(short, long, default_value = "10")]
        limit: u64,
    },
    /// Redact a specific event
    Redact {
        #[arg(short, long, required = true)]
        room_id: OwnedRoomId,

        #[arg(short, long, required = true)]
        event_id: OwnedEventId,

        #[arg(long)]
        reason: Option<String>,
    },
    /// Query room information
    Rooms {
        /// Only query this room
        #[arg(long)]
        room_id: Option<OwnedRoomId>,

        /// Query room members
        #[arg(long = "members")]
        query_members: bool,

        /// Query avatars
        #[arg(long = "avatars")]
        query_avatars: bool,
    },
    /// Send a message to a room
    Send {
        #[arg(short, long, required = true)]
        room_id: OwnedRoomId,

        /// Enable markdown formatting
        #[arg(short, long)]
        markdown: bool,

        /// Send a notice message
        #[arg(short, long)]
        notice: bool,

        /// Send an emote message
        #[arg(short, long, conflicts_with = "notice")]
        emote: bool,

        /// Send file as an attachment
        #[arg(short, long, conflicts_with = "message")]
        attachment: Option<PathBuf>,

        /// Reply to a specific event_id
        #[arg(long, conflicts_with_all = ["notice", "emote", "attachment"])]
        reply_to: Option<OwnedEventId>,

        /// String to send; read from stdin if omitted
        message: Option<String>,
    },
    /// Run sync and print all events
    Sync,
    /// Send typing notifications
    Typing {
        #[arg(long, required = true)]
        room_id: OwnedRoomId,

        /// Disable typing
        #[arg(long)]
        disable: bool,
    },
    /// React to emojic verification requests
    Verify {},
    /// Ask the homeserver who we are
    Whoami,
}

async fn create_client(cmd: &Command) -> anyhow::Result<Client> {
    match cmd {
        Command::Login {
            ref user_id,
            ref device_name,
            password: _,
        } => {
            Client::builder()
                .user_id(user_id.to_owned())
                .device_name(device_name.to_owned())
                .build()
                .await
        }
        Command::Clean { user_id } => Client::builder().user_id(user_id.to_owned()).build().await,
        _ => Client::builder().load_meta()?.build().await?.ensure_login(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    tracing_subscriber::fmt()
        .with_max_level(util::convert_filter(args.verbose.log_level_filter()))
        .init();

    let client = create_client(&args.command).await?;

    match client.clone().sliding_sync {
        Some(s) => {
            let sync = s.sync();
            let mut sync_stream = Box::pin(sync);
            sync_stream.next().await;
        }
        None => {}
    };

    match args.command {
        Command::Clean { .. } => {
            client.clean()?;
        }
        Command::Homeserver {
            force,
            include_token,
        } => {
            let home_server = client.homeserver().to_string();
            let user_id = client.user_id().unwrap().to_string();

            #[derive(Serialize)]
            struct HomeserverOutput {
                home_server: String,
                user_id: String,
                token: Option<String>,
            }

            let mut out = HomeserverOutput {
                home_server,
                user_id,
                token: None,
            };

            if include_token {
                if !force {
                    eprintln!("!!!!!!!!!!!!!!!!!!!!!! WARNING !!!!!!!!!!!!!!!!!!!!!!!!!");
                    eprintln!("!!        Keep this token secret at all times         !!");
                    eprintln!("!! Do not publish it and do not store it as plaintext !!");
                    eprintln!("!!!!!!!!!!!!!!!!!!!!!! WARNING !!!!!!!!!!!!!!!!!!!!!!!!!");
                    eprintln!();
                    eprintln!(
                        "Use -f/--force to display the token if you know what you are doing!"
                    );
                    std::process::exit(1);
                }
                out.token = client.access_token();
            }

            println!("{}", serde_json::to_string(&out)?);
        }
        Command::Login {
            user_id,
            device_name,
            password,
        } => {
            if client.logged_in() {
                bail!("already logged in");
            }

            if session::Meta::exists()? {
                bail!("meta exists");
            }

            let password = match password {
                None => terminal::read_password()?,
                Some(p) => p,
            };

            if let Err(e) = client.login_password(&password).await {
                bail!("login failed: {}", e);
            }

            session::Meta {
                user_id,
                device_name: Some(device_name),
            }
            .dump()?;
        }
        Command::Logout {} => {
            client.logout().await?;
        }
        Command::Messages { room_id, limit } => {
            let msgs = client.messages(room_id, limit).await?;
            let events: Vec<Box<RawValue>> = msgs
                .chunk
                .into_iter()
                .map(|e| e.event.into_json())
                .rev()
                .collect();

            println!("{}", serde_json::to_string(&events)?);
        }
        Command::Rooms {
            room_id,
            query_members,
            query_avatars,
        } => {
            let out = match room_id {
                Some(room_id) => {
                    let Some(room) = client.get_room(&room_id) else {
                        bail!("no such room: {}", room_id);
                    };
                    let output = client
                        .query_room(room, query_avatars, query_members)
                        .await?;
                    serde_json::to_string(&output)?
                }
                None => {
                    let mut output = vec![];
                    for room in client.rooms() {
                        output.push(
                            client
                                .query_room(room, query_avatars, query_members)
                                .await?,
                        );
                    }
                    serde_json::to_string(&output)?
                }
            };

            println!("{}", out);
        }
        Command::Redact {
            room_id,
            event_id,
            reason,
        } => {
            let room = client.get_joined_room(room_id)?;
            room.redact(&event_id, reason.as_ref().map(String::as_ref), None)
                .await?;
        }
        Command::Verify {} => {
            let enc = client.encryption();

            let _e = enc.bootstrap_cross_signing_if_needed(None).await;

            eprintln!("{:#?}", enc.cross_signing_status().await);
            match enc.get_user_identity(client.user_id().unwrap()).await? {
                Some(id) => {
                    eprintln!("{:#?}", id.is_verified());
                    //let _v = id.request_verification().await;
                }
                None => {}
            }
            client.set_sas_handlers().await?;
            client.socket().await?;
        }
        Command::Send {
            room_id,
            reply_to,
            markdown,
            notice,
            emote,
            attachment,
            message,
        } => {
            if let Some(path) = attachment {
                return client.send_attachment(room_id, path).await;
            }

            let body = match message {
                Some(message) => message,
                None => terminal::read_stdin_to_string()?,
            };

            if let Some(ref event_id) = reply_to {
                return client
                    .send_message_reply(room_id, event_id, &body, markdown)
                    .await;
            }

            if notice {
                client.send_notice(room_id, &body, markdown).await?;
            } else if emote {
                client.send_emote(room_id, &body, markdown).await?;
            } else {
                client.send_message(room_id, &body, markdown).await?;
            }
        }
        Command::Sync => {
            client.socket().await?;
        }
        Command::Typing { room_id, disable } => {
            let room = client.get_joined_room(room_id)?;
            room.typing_notice(!disable).await?;
        }
        Command::Whoami => {
            let resp = client.whoami().await?;
            println!("{}", serde_json::to_string(&resp)?);
        }
    };

    Ok(())
}

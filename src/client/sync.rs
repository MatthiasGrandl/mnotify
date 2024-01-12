use std::{
    fmt::Debug,
    fs::File,
    io::{Cursor, Read, Write},
    path::Path,
    time::Duration,
};

use futures::StreamExt;
use matrix_sdk::{
    ruma::{
        api::client::sync::sync_events::v4::RoomSubscription,
        events::{room::EncryptedFile, AnySyncMessageLikeEvent, AnySyncTimelineEvent},
        OwnedEventId, OwnedMxcUri, OwnedRoomId, UInt,
    },
    RoomMemberships,
};
use matrix_sdk_crypto::{AttachmentDecryptor, MediaEncryptionInfo};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    io::{self, AsyncWriteExt, Interest},
    net::{UnixListener, UnixStream},
    sync::watch::{self, Receiver, Sender},
    time::sleep,
};

#[derive(Deserialize, Debug)]
enum SocketCommand {
    #[serde(alias = "send")]
    Send {
        room_id: OwnedRoomId,
        reply_to: Option<OwnedEventId>,
        message: String,
    },
    #[serde(alias = "attachment", alias = "file", alias = "upload")]
    File { room_id: OwnedRoomId, path: String },
    #[serde(alias = "subscribe")]
    Subscribe { room_id: OwnedRoomId },
}

impl super::Client {
    pub(crate) async fn cache_encrypted_file(&self, content: Value) -> anyhow::Result<()> {
        let f = match content.get("file") {
            Some(f) => f,
            None => return Ok(()),
        };
        let file: EncryptedFile = match serde_json::from_value(f.clone()) {
            Ok(file) => file,
            Err(_e) => return Ok(()),
        };
        let (server, id) = file.url.parts().unwrap();
        let path = Path::new("/tmp").join(id);
        if path.is_file() {
            return Ok(());
        }
        let homeserver = self.inner.homeserver();
        let url = format!(
            "{}_matrix/media/v3/download/{}/{}?width=50&height=50&method=scale",
            homeserver, server, id,
        );
        let ciphertext = reqwest::get(url).await.expect("Failed to download file...");
        let ciphertext = ciphertext.bytes().await.unwrap().to_vec();
        let mut cursor = Cursor::new(ciphertext);
        let info: MediaEncryptionInfo = file.into();
        let mut decryptor = AttachmentDecryptor::new(&mut cursor, info)
            .expect("Could not create attachment decryptor");
        let buf = &mut Vec::with_capacity(4096);

        let _size = decryptor.read_to_end(buf)?;
        let mut file = File::create(path)?;
        file.write_all(buf.as_slice())?;
        Ok(())
    }
    pub(crate) fn thumbnail(&self, mxc: Option<OwnedMxcUri>) -> String {
        if mxc.is_none() {
            return String::from("");
        }
        let mxc = mxc.unwrap();
        if !mxc.is_valid() {
            return String::from("");
        }
        let homeserver = self.inner.homeserver();
        format!(
            "{}_matrix/media/v3/thumbnail/{}/{}?width=50&height=50&method=scale",
            homeserver,
            mxc.server_name().unwrap(),
            mxc.media_id().unwrap(),
        )
    }

    pub(crate) fn subscribe(&self, room_id: OwnedRoomId) {
        if self.sliding_sync.is_none() {
            return;
        }
        let mut sub = RoomSubscription::default();
        sub.timeline_limit = Some(UInt::new(30).unwrap());
        self.sliding_sync
            .as_ref()
            .unwrap()
            .subscribe_to_room(room_id, Some(sub));
    }

    async fn socket_command_matcher(&self, command: SocketCommand) -> anyhow::Result<()> {
        eprintln!("{:#?}", command);
        match command {
            SocketCommand::Send {
                room_id,
                reply_to,
                message,
            } => {
                match reply_to {
                    Some(event_id) => {
                        self.send_message_reply(room_id, &event_id, &message, true)
                            .await?
                    }
                    None => self.send_message(room_id, &message, true).await?,
                };
            }
            SocketCommand::File { room_id, path } => {
                self.send_attachment(room_id, path).await?;
            }
            SocketCommand::Subscribe { room_id } => {
                self.subscribe(room_id);
            }
        }
        Ok(())
    }

    pub(crate) async fn socket_command(&self, data: &[u8]) -> anyhow::Result<()> {
        let iter = &mut data.split(|b| b.eq(&b'\n'));
        loop {
            let Some(data) = iter.next() else {
                break;
            };
            let self_clone = self.clone();
            let c = serde_json::from_slice::<SocketCommand>(&data)?;
            tokio::spawn(async move {
                let _r = self_clone.socket_command_matcher(c).await;
            });
        }
        Ok(())
    }

    pub(crate) async fn handle_client(
        &self,
        mut r: Receiver<Vec<u8>>,
        mut s: UnixStream,
    ) -> anyhow::Result<()> {
        let self_clone = self.clone();
        tokio::spawn(async move {
            loop {
                let ready = s
                    .ready(Interest::READABLE | Interest::WRITABLE)
                    .await
                    .unwrap();
                if ready.is_readable() {
                    let mut data = vec![0; 4096];
                    // Try to read data, this may still fail with `WouldBlock`
                    // if the readiness event is a false positive.
                    match s.try_read(&mut data) {
                        Ok(n) => {
                            if n == 0 {
                                break;
                            }
                            Some(self_clone.socket_command(&data[0..n]).await);
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            continue;
                        }
                        Err(_e) => break,
                    }
                }
                if ready.is_writable() {
                    let changed = r.has_changed();
                    if changed.is_err() {
                        break;
                    }
                    if !changed.unwrap() {
                        sleep(Duration::from_millis(100)).await;
                        continue;
                    }

                    let mut json = r.borrow_and_update().clone();
                    json.push(b'\n');
                    // this prints the correct state:
                    // println!("{}", String::from_utf8(json.clone()).unwrap());

                    // this prints the state from the previous iteration, why???
                    let res = s.write_all(&json).await;
                    if res.is_err() {
                        continue;
                    }
                }
            }
        });
        Ok(())
    }

    pub(crate) async fn handle_connections(&self, r: Receiver<Vec<u8>>) -> anyhow::Result<()> {
        let path = Path::new("/tmp/mnotify.sock");
        if path.exists() && std::fs::remove_file(path).is_err() {
            eprintln!("Socket is still open!");
            return Ok(());
        }
        let listener = UnixListener::bind(path).unwrap();
        loop {
            match listener.accept().await {
                Ok((s, _addr)) => {
                    Some(self.handle_client(r.clone(), s).await);
                }
                Err(_e) => { /* connection failed */ }
            }
        }
    }

    pub(crate) async fn handle_stream(&self, s: Sender<Vec<u8>>) -> anyhow::Result<()> {
        let mut json = vec![];
        loop {
            let ss = match &self.sliding_sync {
                Some(s) => s,
                None => panic!("no sliding sync"),
            };
            let sync = ss.sync();
            let mut sync_stream = Box::pin(sync);
            while let Some(Ok(_response)) = sync_stream.next().await {
                let mut output = vec![];
                let rooms = ss.get_all_rooms().await;
                for room in rooms {
                    let r = self.inner.get_room(room.room_id());
                    if r.is_none() {
                        continue;
                    }
                    let r = r.unwrap();

                    let events = room
                        .timeline_queue()
                        .iter()
                        .filter_map(|ev|{
                            let m = ev.event.deserialize_as::<AnySyncTimelineEvent>();
                            if m.is_err() {
                                return None;
                            }
                            let m = m.unwrap();
                            //eprintln!("{:#?}", result);
                            match m {
                                AnySyncTimelineEvent::MessageLike(e) => {
                                    match e {
                                        AnySyncMessageLikeEvent::RoomMessage(e) => {
                                            match e {
                                                matrix_sdk::ruma::events::SyncMessageLikeEvent::Original(e) => {
                                            if ev.encryption_info.is_some() {
                                                let json = match serde_json::to_value(e.content) {
                                                    Ok(j) => j,
                                                    Err(_e) => return None,
                                                };
                                                        let this = self.clone();
                                                tokio::spawn(async move {
                                                        let _r = this.cache_encrypted_file(json).await;
                                                });
                                            }
                                                },
                                                _ => return None,
                                            }
                                        }
                                        _ => return None,
                                    }
                                }
                                _ => {
                                    return None;
                                }
                            }

                            Some(ev.clone())
                        })
                        .collect::<Vec<_>>();

                    let mut members = vec![];

                    for member in r.members(RoomMemberships::ACTIVE).await? {
                        let avatar = match member.avatar_url() {
                            Some(uri) => self.thumbnail(Some(OwnedMxcUri::from(uri))),
                            None => String::from(""),
                        };
                        members.push(crate::outputs::RoomMember {
                            avatar,
                            name: member.name().to_string(),
                            display_name: member.display_name().map(|s| s.to_string()),
                            user_id: member.user_id().to_string(),
                        })
                    }
                    let o = crate::outputs::SSRoom {
                        room_id: room.room_id().to_string(),
                        name: room.name(),
                        events,
                        members,
                        unread_notifications: room.unread_notifications(),
                        avatar: self.thumbnail(room.avatar_url()),
                        is_direct: room.is_dm().unwrap_or(false),
                    };

                    output.push(o);
                }

                //println!("{}", serde_json::to_string(&output).unwrap());
                let new_json = serde_json::to_vec(&output).unwrap();
                if new_json != json {
                    json = new_json;
                    s.send(json.clone()).unwrap();
                }
            }
            eprintln!("Sync stream ended");
        }
    }
    pub(crate) async fn socket(&self) -> anyhow::Result<()> {
        let (s, r) = watch::channel::<Vec<u8>>(vec![]);
        let self_clone = self.clone();
        tokio::spawn(async move {
            Some(self_clone.handle_connections(r).await);
        });
        self.handle_stream(s).await?;
        Ok(())
    }
}

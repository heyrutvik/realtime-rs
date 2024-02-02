use serde_json::Value;
use tokio::sync::{
    mpsc::{self, error::SendError, UnboundedReceiver, UnboundedSender},
    Mutex,
};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::message::{
    payload::{
        AccessTokenPayload, BroadcastConfig, BroadcastPayload, JoinConfig, JoinPayload, Payload,
        PayloadStatus, PostgresChange, PostgresChangesEvent, PostgresChangesPayload,
        PresenceConfig,
    },
    presence::{PresenceCallback, PresenceEvent, PresenceState},
    MessageEvent, PostgresChangeFilter, RealtimeMessage,
};

use crate::sync::{realtime_client::RealtimeClient, realtime_presence::RealtimePresence};
use std::fmt::Debug;
use std::{collections::HashMap, sync::Arc};

type CdcCallback = (
    PostgresChangeFilter,
    Box<dyn FnMut(&PostgresChangesPayload) + Send>,
);
type BroadcastCallback = Box<dyn FnMut(&HashMap<String, Value>) + Send>;

pub enum ChannelControlMessage {
    Subscribe,
    Broadcast(BroadcastPayload),
    ClientTx(UnboundedSender<Message>),
}

/// Channel states
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum ChannelState {
    Closed,
    Errored,
    Joined,
    Joining,
    Leaving,
}

/// Error for channel send failures
#[derive(Debug)]
pub enum ChannelSendError {
    NoChannel,
    SendError(SendError<Message>),
    ChannelError(ChannelState),
}

/// Channel structure
pub struct RealtimeChannel {
    pub(crate) topic: String,
    pub(crate) state: Arc<Mutex<ChannelState>>,
    pub(crate) id: Uuid,
    pub(crate) cdc_callbacks: Arc<Mutex<HashMap<PostgresChangesEvent, Vec<CdcCallback>>>>,
    pub(crate) broadcast_callbacks: Arc<Mutex<HashMap<String, Vec<BroadcastCallback>>>>,
    pub(crate) client_tx: mpsc::UnboundedSender<Message>,
    join_payload: JoinPayload,
    presence: RealtimePresence,
    pub(crate) tx: Option<UnboundedSender<Message>>,
    pub controller: (
        UnboundedSender<ChannelControlMessage>,
        UnboundedReceiver<ChannelControlMessage>,
    ),
}

impl RealtimeChannel {
    /// Returns the channel's connection state
    pub async fn get_status(&self) -> ChannelState {
        let state = self.state.lock().await;
        let s = state.clone();
        drop(state);
        s
    }

    /// Send a join request to the channel
    pub async fn subscribe(&mut self) {
        let join_message = RealtimeMessage {
            event: MessageEvent::PhxJoin,
            topic: self.topic.clone(),
            payload: Payload::Join(self.join_payload.clone()),
            message_ref: Some(self.id.into()),
        };

        let mut state = self.state.lock().await;
        *state = ChannelState::Joining;
        drop(state);

        let _ = self.send(join_message.into()).await;
    }

    pub async fn start_thread(&mut self) {
        let (channel_tx, mut channel_rx) = mpsc::unbounded_channel::<Message>();
        self.tx = Some(channel_tx);
        let thread_state = self.state.clone();
        let thread_cdc_cbs = self.cdc_callbacks.clone();
        let thread_bc_cbs = self.broadcast_callbacks.clone();
        let id = self.id;

        let _ws_thread = tokio::spawn(async move {
            while let Some(message) = channel_rx.recv().await {
                let message: RealtimeMessage =
                    serde_json::from_str(message.to_text().unwrap()).unwrap();

                // get locks
                let mut broadcast_callbacks = thread_bc_cbs.lock().await;
                let mut cdc_callbacks = thread_cdc_cbs.lock().await;

                let test_message = message.clone(); // TODO fix dis

                match message.payload {
                    Payload::Broadcast(payload) => {
                        if let Some(cb_vec) = broadcast_callbacks.get_mut(&payload.event) {
                            for cb in cb_vec {
                                cb(&payload.payload);
                            }
                        }
                    }
                    Payload::PostgresChanges(payload) => {
                        if let Some(cb_vec) = cdc_callbacks.get_mut(&payload.data.change_type) {
                            for cb in cb_vec {
                                if cb.0.check(test_message.clone()).is_none() {
                                    continue;
                                }
                                cb.1(&payload);
                            }
                        }
                        if let Some(cb_vec) = cdc_callbacks.get_mut(&PostgresChangesEvent::All) {
                            for cb in cb_vec {
                                if cb.0.check(test_message.clone()).is_none() {
                                    continue;
                                }
                                cb.1(&payload);
                            }
                        }
                    }
                    Payload::Response(join_response) => {
                        let target_id = message.message_ref.clone().unwrap_or("".to_string());
                        if target_id != id.to_string() {
                            return;
                        }
                        if join_response.status == PayloadStatus::Ok {
                            let mut channel_state = thread_state.lock().await;
                            *channel_state = ChannelState::Joined;
                            drop(channel_state);
                        }
                    }
                    _ => {
                        println!("Unmatched payload ;_;")
                    }
                }

                drop(broadcast_callbacks);
                drop(cdc_callbacks);
            }
        });
    }

    pub async fn run_controller(&mut self) {
        // CONTROLLER

        while let Some(control_message) = self.controller.1.recv().await {
            match control_message {
                ChannelControlMessage::Subscribe => self.subscribe().await,
                ChannelControlMessage::Broadcast(payload) => {
                    let _ = self.broadcast(payload).await;
                }
                ChannelControlMessage::ClientTx(tx) => self.client_tx = tx,
            }
        }
    }

    /// Leave the channel
    async fn unsubscribe(&mut self) -> Result<ChannelState, ChannelSendError> {
        let state = self.state.clone();
        let mut state = state.lock().await;
        if *state == ChannelState::Closed || *state == ChannelState::Leaving {
            let s = state.clone();
            return Ok(s);
        }

        match self
            .send(RealtimeMessage {
                event: MessageEvent::PhxLeave,
                topic: self.topic.clone(),
                payload: Payload::Empty {},
                message_ref: Some(format!("{}+leave", self.id)),
            })
            .await
        {
            Ok(()) => {
                *state = ChannelState::Leaving;
                Ok(*state)
            }
            Err(ChannelSendError::ChannelError(status)) => Ok(status),
            Err(e) => Err(e),
        }
    }

    /// Returns the current [PresenceState] of the channel
    pub fn presence_state(&self) -> PresenceState {
        self.presence.state.clone()
    }

    /// Track provided state in Realtime Presence
    /// ```
    /// # use std::{collections::HashMap, env};
    /// # use realtime_rs::sync::*;
    /// # use realtime_rs::message::*;  
    /// # use realtime_rs::*;          
    /// # fn main() -> Result<(), ()> {
    /// #   let url = "http://127.0.0.1:54321";
    /// #   let anon_key = env::var("LOCAL_ANON_KEY").expect("No anon key!");
    /// #
    /// #   let mut client = RealtimeClient::builder(url, anon_key)
    /// #       .build();
    /// #
    /// #   match client.connect() {
    /// #       Ok(_) => {}
    /// #       Err(e) => panic!("Couldn't connect! {:?}", e),
    /// #   };
    /// #
    /// #   let channel_id = client.channel("topic").build(&mut client);
    /// #
    /// #   let _ = client.block_until_subscribed(channel_id);
    /// #
    ///     client
    ///         .get_channel_mut(channel_id)
    ///         .unwrap()
    ///         .track(HashMap::new());
    /// #   Ok(())
    /// #   }
    pub fn track(&mut self, payload: HashMap<String, Value>) -> &mut RealtimeChannel {
        let _ = self.send(RealtimeMessage {
            event: MessageEvent::Presence,
            topic: self.topic.clone(),
            payload: Payload::PresenceTrack(payload.into()),
            message_ref: None,
        });

        self
    }

    /// Sends a message to stop tracking this channel's presence
    pub fn untrack(&mut self) {
        let _ = self.send(RealtimeMessage {
            event: MessageEvent::Untrack,
            topic: self.topic.clone(),
            payload: Payload::Empty {},
            message_ref: None,
        });
    }

    /// Send a [RealtimeMessage] on this channel
    pub async fn send(&mut self, message: RealtimeMessage) -> Result<(), ChannelSendError> {
        // inject channel topic to message here
        let mut message = message.clone();
        message.topic = self.topic.clone();

        let state = self.state.lock().await;

        if *state == ChannelState::Leaving {
            return Err(ChannelSendError::ChannelError(state.clone()));
        }

        match self.client_tx.send(message.into()) {
            Ok(()) => Ok(()),
            Err(e) => Err(ChannelSendError::SendError(e)),
        }
    }

    /// Helper function for sending broadcast messages
    ///```
    ///TODO CODE
    async fn broadcast(&mut self, payload: BroadcastPayload) -> Result<(), ChannelSendError> {
        self.send(RealtimeMessage {
            event: MessageEvent::Broadcast,
            topic: "".into(),
            payload: Payload::Broadcast(payload),
            message_ref: None,
        })
        .await
    }

    pub(crate) async fn set_auth(&mut self, access_token: String) -> Result<(), ChannelSendError> {
        self.join_payload.access_token = access_token.clone();

        let state = self.state.lock().await;

        if *state != ChannelState::Joined {
            return Ok(());
        }

        drop(state);

        let access_token_message = RealtimeMessage {
            event: MessageEvent::AccessToken,
            topic: self.topic.clone(),
            payload: Payload::AccessToken(AccessTokenPayload { access_token }),
            ..Default::default()
        };

        self.send(access_token_message).await
    }

    // pub(crate) fn recieve(&mut self, message: RealtimeMessage) {
    //     match &message.payload {
    //         Payload::Response(join_response) => {
    //             let target_id = message.message_ref.clone().unwrap_or("".to_string());
    //             if target_id != self.id.to_string() {
    //                 return;
    //             }
    //             if join_response.status == PayloadStatus::Ok {
    //                 self.status = ChannelState::Joined;
    //             }
    //         }
    //         Payload::PresenceState(state) => self.presence.sync(state.clone().into()),
    //         Payload::PresenceDiff(raw_diff) => {
    //             self.presence.sync_diff(raw_diff.clone().into());
    //         }
    //         Payload::PostgresChanges(payload) => {
    //             let event = &payload.data.change_type;
    //
    //             for cdc_callback in self.cdc_callbacks.get_mut(event).unwrap_or(&mut vec![]) {
    //                 let filter = &cdc_callback.0;
    //
    //                 // TODO REFAC pointless message clones when not using result; filter.check
    //                 // should borrow and return bool/result
    //                 if let Some(_message) = filter.check(message.clone()) {
    //                     cdc_callback.1(payload);
    //                 }
    //             }
    //
    //             for cdc_callback in self
    //                 .cdc_callbacks
    //                 .get_mut(&PostgresChangesEvent::All)
    //                 .unwrap_or(&mut vec![])
    //             {
    //                 let filter = &cdc_callback.0;
    //
    //                 if let Some(_message) = filter.check(message.clone()) {
    //                     cdc_callback.1(payload);
    //                 }
    //             }
    //         }
    //         Payload::Broadcast(payload) => {
    //             if let Some(callbacks) = self.broadcast_callbacks.get_mut(&payload.event) {
    //                 for cb in callbacks {
    //                     cb(&payload.payload);
    //                 }
    //             }
    //         }
    //         _ => {}
    //     }
    //
    //     match &message.event {
    //         MessageEvent::PhxClose => {
    //             if let Some(message_ref) = message.message_ref {
    //                 if message_ref == self.id.to_string() {
    //                     self.status = ChannelState::Closed;
    //                     if DEBUG {
    //                         println!("Channel Closed! {:?}", self.id);
    //                     }
    //                 }
    //             }
    //         }
    //         MessageEvent::PhxReply => {
    //             if message.message_ref.clone().unwrap_or("#NOREF".to_string())
    //                 == format!("{}+leave", self.id)
    //             {
    //                 self.status = ChannelState::Closed;
    //                 if DEBUG {
    //                     println!("Channel Closed! {:?}", self.id);
    //                 }
    //             }
    //         }
    //         _ => {}
    //     }
    // }
}

impl Debug for RealtimeChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!(
            "RealtimeChannel {{ name: {:?}, callbacks: [TODO DEBUG]}}",
            self.topic
        ))
    }
}

/// Builder struct for [RealtimeChannel]
///
/// Get access to this through [RealtimeClient::channel()]
pub struct RealtimeChannelBuilder {
    topic: String,
    access_token: String,
    broadcast: BroadcastConfig,
    presence: PresenceConfig,
    id: Uuid,
    postgres_changes: Vec<PostgresChange>,
    cdc_callbacks: HashMap<PostgresChangesEvent, Vec<CdcCallback>>,
    broadcast_callbacks: HashMap<String, Vec<BroadcastCallback>>,
    presence_callbacks: HashMap<PresenceEvent, Vec<PresenceCallback>>,
    client_tx: mpsc::UnboundedSender<Message>,
}

impl RealtimeChannelBuilder {
    pub(crate) fn new(client: &mut RealtimeClient) -> Self {
        Self {
            topic: "no_topic".into(),
            access_token: client.access_token.clone(),
            broadcast: Default::default(),
            presence: Default::default(),
            id: Uuid::new_v4(),
            postgres_changes: Default::default(),
            cdc_callbacks: Default::default(),
            broadcast_callbacks: Default::default(),
            presence_callbacks: Default::default(),
            client_tx: client.get_channel_tx(),
        }
    }

    /// Set the topic of the channel
    pub fn topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = format!("realtime:{}", topic.into());
        self
    }

    /// Set the broadcast config for this channel
    pub fn broadcast(mut self, broadcast_config: BroadcastConfig) -> Self {
        self.broadcast = broadcast_config;
        self
    }

    /// Set the presence config for this channel
    pub fn presence(mut self, presence_config: PresenceConfig) -> Self {
        self.presence = presence_config;
        self
    }

    /// Add a postgres changes callback to this channel
    ///```
    /// # use realtime_rs::sync::*;
    /// # use realtime_rs::message::*;  
    /// # use realtime_rs::message::payload::*;  
    /// # use realtime_rs::*;          
    /// # use std::env;
    /// #
    /// # fn main() -> Result<(), ()> {
    /// #     let url = "http://127.0.0.1:54321";
    /// #     let anon_key = env::var("LOCAL_ANON_KEY").expect("No anon key!");
    /// #     let mut client = RealtimeClient::builder(url, anon_key).build();
    /// #     let _ = client.connect();
    ///
    ///     let my_pgc_callback = move |msg: &_| {
    ///         println!("Got message: {:?}", msg);
    ///     };
    ///
    ///     let channel_id = client
    ///         .channel("topic")
    ///         .on_postgres_change(
    ///             PostgresChangesEvent::All,
    ///             PostgresChangeFilter {
    ///                 schema: "public".into(),
    ///                 table: Some("todos".into()),
    ///                 ..Default::default()
    ///             },
    ///             my_pgc_callback,
    ///         )
    ///         .build(&mut client);
    /// #
    /// #     client.get_channel_mut(channel_id).unwrap().subscribe();
    /// #     loop {
    /// #         if client.get_status() == ConnectionState::Closed {
    /// #             break;
    /// #         }
    /// #         match client.next_message() {
    /// #             Ok(_topic) => return Ok(()),
    /// #             Err(NextMessageError::WouldBlock) => return Ok(()),
    /// #             Err(_e) => return Err(()),
    /// #         }
    /// #     }
    /// #     Err(())
    /// # }
    pub fn on_postgres_change(
        mut self,
        event: PostgresChangesEvent,
        filter: PostgresChangeFilter,
        callback: impl FnMut(&PostgresChangesPayload) + 'static + Send,
    ) -> Self {
        self.postgres_changes.push(PostgresChange {
            event: event.clone(),
            schema: filter.schema.clone(),
            table: filter.table.clone().unwrap_or("".into()),
            filter: filter.filter.clone(),
        });

        if self.cdc_callbacks.get_mut(&event).is_none() {
            self.cdc_callbacks.insert(event.clone(), vec![]);
        }

        self.cdc_callbacks
            .get_mut(&event)
            .unwrap_or(&mut vec![])
            .push((filter, Box::new(callback)));

        self
    }

    /// Add a presence callback to this channel
    ///```
    /// # use realtime_rs::sync::*;
    /// # use realtime_rs::message::*;  
    /// # use realtime_rs::*;          
    /// # use std::env;
    /// #
    /// # fn main() -> Result<(), ()> {
    /// #     let url = "http://127.0.0.1:54321";
    /// #     let anon_key = env::var("LOCAL_ANON_KEY").expect("No anon key!");
    /// #     let mut client = RealtimeClient::builder(url, anon_key).build();
    /// #     let _ = client.connect();
    ///
    ///     let channel_id = client
    ///         .channel("topic".to_string())
    ///         .on_presence(PresenceEvent::Sync, |key, old_state, new_state| {
    ///             println!("Presence sync: {:?}, {:?}, {:?}", key, old_state, new_state);
    ///         })
    ///         .build(&mut client);
    ///
    /// #     client.get_channel_mut(channel_id).unwrap().subscribe();
    /// #     loop {
    /// #         if client.get_status() == ConnectionState::Closed {
    /// #             break;
    /// #         }
    /// #         match client.next_message() {
    /// #             Ok(_topic) => return Ok(()),
    /// #             Err(NextMessageError::WouldBlock) => return Ok(()),
    /// #             Err(_e) => return Err(()),
    /// #         }
    /// #     }
    /// #     Err(())
    /// # }
    pub fn on_presence(
        mut self,
        event: PresenceEvent,
        // TODO callback type alias
        callback: impl FnMut(String, PresenceState, PresenceState) + 'static + Send,
    ) -> Self {
        if self.presence_callbacks.get_mut(&event).is_none() {
            self.presence_callbacks.insert(event.clone(), vec![]);
        }

        self.presence_callbacks
            .get_mut(&event)
            .unwrap_or(&mut vec![])
            .push(Box::new(callback));

        self
    }

    /// Add a broadcast callback to this channel
    /// ```
    /// # use realtime_rs::sync::*;
    /// # use realtime_rs::message::*;  
    /// # use realtime_rs::*;          
    /// # use std::env;
    /// #
    /// # fn main() -> Result<(), ()> {
    /// #     let url = "http://127.0.0.1:54321";
    /// #     let anon_key = env::var("LOCAL_ANON_KEY").expect("No anon key!");
    /// #     let mut client = RealtimeClient::builder(url, anon_key).build();
    /// #     let _ = client.connect();
    ///
    ///     let channel_id = client
    ///         .channel("topic")
    ///         .on_broadcast("subtopic", |msg| {
    ///             println!("recieved broadcast: {:?}", msg);
    ///         })
    ///         .build(&mut client);
    ///
    /// #     client.get_channel_mut(channel_id).unwrap().subscribe();
    /// #     loop {
    /// #         if client.get_status() == ConnectionState::Closed {
    /// #             break;
    /// #         }
    /// #         match client.next_message() {
    /// #             Ok(_topic) => return Ok(()),
    /// #             Err(NextMessageError::WouldBlock) => return Ok(()),
    /// #             Err(_e) => return Err(()),
    /// #         }
    /// #     }
    /// #     Err(())
    /// # }
    pub fn on_broadcast(
        mut self,
        event: impl Into<String>,
        callback: impl FnMut(&HashMap<String, Value>) + 'static + Send,
    ) -> Self {
        let event: String = event.into();

        if self.broadcast_callbacks.get_mut(&event).is_none() {
            self.broadcast_callbacks.insert(event.clone(), vec![]);
        }

        self.broadcast_callbacks
            .get_mut(&event)
            .unwrap_or(&mut vec![])
            .push(Box::new(callback));

        self
    }

    // TODO on_message handler for sys messages

    /// Create the channel and pass ownership to provided [RealtimeClient], returning the channel
    /// id for later access through the client
    pub async fn build(
        self,
        client: &mut RealtimeClient,
    ) -> UnboundedSender<ChannelControlMessage> {
        let state = Arc::new(Mutex::new(ChannelState::Closed));
        let cdc_callbacks = Arc::new(Mutex::new(self.cdc_callbacks));
        let broadcast_callbacks = Arc::new(Mutex::new(self.broadcast_callbacks));
        let (controller_tx, controller_rx) = mpsc::unbounded_channel::<ChannelControlMessage>();

        let mut c = RealtimeChannel {
            tx: None,
            topic: self.topic,
            cdc_callbacks,
            broadcast_callbacks,
            client_tx: self.client_tx,
            state,
            id: self.id,
            join_payload: JoinPayload {
                config: JoinConfig {
                    broadcast: self.broadcast,
                    presence: self.presence,
                    postgres_changes: self.postgres_changes,
                },
                access_token: self.access_token,
            },
            presence: RealtimePresence::from_channel_builder(self.presence_callbacks),
            controller: (controller_tx, controller_rx),
        };

        c.start_thread().await;

        client.add_channel(c).await
    }
}

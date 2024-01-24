use std::{collections::HashMap, env};

use realtime_rs::{
    message::presence::PresenceEvent,
    sync::{ChannelState, ConnectionState, NextMessageError, RealtimeClient},
};

fn main() {
    let url = "http://127.0.0.1:54321";
    let anon_key = env::var("LOCAL_ANON_KEY").expect("No anon key!");

    let mut client = RealtimeClient::builder(url, anon_key).build();

    let client = match client.connect() {
        Ok(client) => client,
        Err(e) => panic!("Couldn't connect! {:?}", e), // TODO retry routine
    };

    let mut presence_payload = HashMap::new();
    presence_payload.insert("kb_layout".into(), "en_US".into());

    let channel_id = client
        .channel("channel_1".to_string())
        // TODO presence_state message event
        .on_presence(PresenceEvent::Sync, |key, _old_state, _new_state| {
            println!("Presence sync: {:?}", key);
        })
        .on_presence(PresenceEvent::Join, |key, _old_state, _new_state| {
            println!("Presence join: {:?}", key);
        })
        .on_presence(PresenceEvent::Leave, |key, _old_state, _new_statee| {
            println!("Presence leave: {:?}", key);
        })
        .build(client);

    client.get_channel_mut(channel_id).unwrap().subscribe();

    let mut sent_once = false;

    loop {
        if client.get_status() == ConnectionState::Closed {
            break;
        }

        match client.next_message() {
            Ok(topic) => {
                println!("Message forwarded to {:?}", topic)
            }
            Err(NextMessageError::WouldBlock) => {}
            Err(_e) => {
                //println!("NextMessageError: {:?}", e)
            }
        }

        if sent_once {
            continue;
        }

        let channel = client.get_channel_mut(channel_id).unwrap();

        if channel.get_status() == ChannelState::Joined {
            channel.track(presence_payload.clone());
            sent_once = true;
        }
    }

    println!("Client closed.");
}

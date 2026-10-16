//! Print each new prompt as the server finds it.
//!
//! ```sh
//! RECAP_URL=http://127.0.0.1:7718 cargo run -p recap-client --example tail
//! ```
//!
//! The example reads the change events with [`recap_client::watch`]. For each
//! `changed` event, it asks the server for the entries that changed after the
//! last version that it saw.

use recap_api::{Api, EntryFields, Event, Filter, KindFilter, Page};
use recap_client::{Client, Message};
use std::sync::mpsc;

fn main() {
    let client = Client::from_env();
    let prompts = Filter {
        kind: KindFilter::Prompts,
        ..Filter::default()
    };
    let started = std::time::Instant::now();
    let all = match client.history(&prompts, &Page::default()) {
        Ok(all) => all,
        Err(e) => {
            eprintln!("{}: {e}", client.base());
            std::process::exit(1);
        }
    };
    let count: usize = all.groups.iter().map(|g| g.entries.len()).sum();
    eprintln!(
        "{count} prompts at version {} ({:?}). Waiting for new prompts…",
        all.version,
        started.elapsed()
    );

    let mut version = all.version;
    let (tx, rx) = mpsc::channel();
    recap_client::watch(client.clone(), tx);
    for message in rx {
        match message {
            Message::Connected => eprintln!("connected to {}", client.base()),
            Message::Disconnected(error) => eprintln!("{error}"),
            Message::Event(Event::Changed { version: next }) => {
                let page = Page {
                    since: Some(version),
                    ..Page::default()
                };
                match client.history(&prompts, &page) {
                    Ok(changed) => {
                        for entry in changed.groups.iter().flat_map(|g| &g.entries) {
                            let text = entry.prompt_text().unwrap_or_default();
                            let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
                            let line: String = line.chars().take(100).collect();
                            println!("{}  {line}", entry.timestamp.format("%H:%M:%S"));
                        }
                        version = next;
                    }
                    Err(e) => eprintln!("{e}"),
                }
            }
            Message::Event(_) => {}
        }
    }
}

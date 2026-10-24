//! Print tool calls and their results as they happen, across every session.
//!
//! ```sh
//! cargo run -p uniflo-client --example watch
//! ```
//!
//! Needs a running `uniflo daemon` (`cargo run -p uniflo -- daemon`, or the installed `uniflo`
//! binary). Reads `UNIFLO_URL` / `UNIFLO_TOKEN` the same way the CLI does.

use uniflo_client::uniflo_schema::Body;
use uniflo_client::{Client, Envelope, StreamOptions};

fn main() -> Result<(), uniflo_client::Error> {
    let client = Client::local()?;
    eprintln!(
        "watching tool calls on {:?}; Ctrl-C to stop",
        std::env::var("UNIFLO_URL").as_deref().unwrap_or_default()
    );

    let opts = StreamOptions { kinds: Some("tool_call,tool_result".to_owned()), ..Default::default() };
    for envelope in client.stream(&opts)? {
        let Envelope::Event { event, .. } = envelope? else { continue };
        match event.body {
            Body::ToolCall { name, input, .. } => {
                println!("[{}] -> {name} {input}", event.session);
            }
            Body::ToolResult { name, is_error, output, .. } => {
                let name = name.unwrap_or_else(|| "?".to_owned());
                let mark = if is_error { "error" } else { "ok" };
                let preview: String = output.chars().take(120).collect();
                println!("[{}] <- {name} ({mark}) {preview}", event.session);
            }
            _ => {}
        }
    }
    Ok(())
}

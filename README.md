# web-ws

WASM library to use JavaScript's WebSockets easly.

## Usage

```rust
use web_ws::WSStream;

#[wasm_bindgen(start)]
pub async fn start() -> Result<(), JsValue> {
	let ws = WSStream::connect(&"ws://localhost:8080/foo").await;
	if let Ok(ws) = ws {
		let (mut sender, mut receiver) = ws.split();
		sender.send(b"test123").await.ok();
		while let Some(data) = receiver.next().await {
			console_log!("{:?}", data);
			// if ... { sender.close(); }
		}
	}
	Ok(())
}

```

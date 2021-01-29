use std::cell::RefCell;
use std::rc::Rc;
use std::pin::Pin;
use bytes::{ Bytes, BytesMut };
use futures::channel::mpsc::{ unbounded, UnboundedReceiver, UnboundedSender };
use futures::channel::oneshot;
use futures::task::{ Poll, Context, Waker }; 
use futures::future::{self, Either};
use futures::SinkExt;
use futures::stream::StreamExt;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{ErrorEvent, MessageEvent, WebSocket};

#[wasm_bindgen]
extern "C" {
	#[wasm_bindgen(js_namespace = console)]
	fn log(s: &str);
}

macro_rules! console_log {
    ($($t:tt)*) => (log(&format_args!($($t)*).to_string()))
}

#[derive(Debug, PartialEq)]
enum State {
	Connected,
    Closed,
    Errored(ErrorEvent)
}

#[derive(Debug)]
pub enum Error {
	Any,
	CloseSocket,
	Js(ErrorEvent)
}

async fn bg_thread(buffer: Rc<RefCell<BytesMut>>, waker: Rc<RefCell<Option<Waker>>>, state: Rc<RefCell<State>>, internal_receiver: UnboundedReceiver<Bytes>, close_receiver: oneshot::Receiver<bool>) {
	/* Look if we can put this logic only in the callbacks and not create a local thread */
	let receiver = internal_receiver.map(|data| {
		waker.borrow().as_ref().map(|waker| waker.wake_by_ref());
		buffer.borrow_mut().extend_from_slice(&data);
	}).collect::<()>();
	match future::select(receiver, close_receiver).await {
		Either::Left((_, _)) => {
			*state.borrow_mut() = State::Closed;
		},
		Either::Right((_, _)) => {
			*state.borrow_mut() = State::Closed;
		}
	};
	waker.borrow().as_ref().map(|waker| waker.wake_by_ref());
}

#[derive(Debug)]
pub struct WSStream {
	ws: WebSocket,
    on_message_cb: Closure<dyn FnMut (MessageEvent)>,
    on_error_cb: Closure<dyn FnMut (ErrorEvent)>,
    on_close_cb: Closure<dyn FnMut (JsValue)>,
	state: Rc<RefCell<State>>,
	read_waker: Rc<RefCell<Option<Waker>>>,
	buffer: Rc<RefCell<BytesMut>>,
}

impl Drop for WSStream {
    fn drop(&mut self) {
		self.ws.set_onclose(None);
		self.on_close_cb.as_ref().unchecked_ref::<js_sys::Function>().call0(&wasm_bindgen::JsValue::NULL).ok();
		self.ws.close().ok();
    }
}

impl WSStream {
    pub async fn connect(url: &str) -> Result<WSStream, Error> {
        let ws = WebSocket::new(url).map_err(|_| Error::Any)?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
        let (internal_sender, internal_receiver): (UnboundedSender<Bytes>, UnboundedReceiver<Bytes>) = unbounded::<Bytes>();

        // On Message
        let on_message_cb = Closure::wrap(Box::new(move |e: MessageEvent| {
			if let Ok(abuf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
				let array = js_sys::Uint8Array::new(&abuf);
				internal_sender.unbounded_send(Bytes::from(array.to_vec())).ok();
			} else if let Ok(txt) = e.data().dyn_into::<js_sys::JsString>() {
				internal_sender.unbounded_send(Bytes::from(String::from(&txt))).ok();
			}
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(on_message_cb.as_ref().unchecked_ref()));

        // On close
		let (send_close, close_receiver) = oneshot::channel();
		let mut send_close = Some(send_close);
        let on_close_cb = Closure::wrap(Box::new(move |_| {
			send_close.take().map(|sender| sender.send(true).ok());
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onclose(Some(on_close_cb.as_ref().unchecked_ref()));

        let (send_error_connect, recv_error_connect) = oneshot::channel::<ErrorEvent>();
		let mut send_error_connect = Some(send_error_connect);
        // On Connect Error
        let on_connect_error_cb = Closure::wrap(Box::new(move |e: ErrorEvent| {
			send_error_connect.take().map(|sender| sender.send(e).ok());
        }) as Box<dyn FnMut(ErrorEvent)>);
        ws.set_onerror(Some(on_connect_error_cb.as_ref().unchecked_ref()));
		// On Connect OK
		let (send_ok_connect, recv_ok_connect) = oneshot::channel::<bool>();
		let mut send_ok_connect = Some(send_ok_connect);
        let on_open_cb = Closure::wrap(Box::new(move |_| {
			send_ok_connect.take().map(|sender| sender.send(true).ok());
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onopen(Some(on_open_cb.as_ref().unchecked_ref()));
		// Wait for the connection ok or err
		match future::select(recv_ok_connect, recv_error_connect).await {
			Either::Left((_, _)) => { /* Received something on recv_ok_connect */ },
			Either::Right((e, _)) => {
				ws.set_onclose(None);
				return Err(Error::Js(e.unwrap())); // Err if sender droped, not the case here
			}
		}
        ws.set_onopen(None);

		let state = Rc::new(RefCell::new(State::Connected));
		let state_cl = state.clone();
        // On error
        let on_error_cb = Closure::wrap(Box::new(move |e: ErrorEvent| {
			*state_cl.borrow_mut() = State::Errored(e); 
        }) as Box<dyn FnMut(ErrorEvent)>);
		ws.set_onerror(Some(on_error_cb.as_ref().unchecked_ref()));

		let buffer = Rc::new(RefCell::new(BytesMut::new()));
		let read_waker = Rc::new(RefCell::new(None));

		/* Spawn local thread */
		spawn_local(bg_thread(buffer.clone(), read_waker.clone(), state.clone(), internal_receiver, close_receiver));

        Ok(WSStream {
			ws,
            on_error_cb,
            on_message_cb,
			on_close_cb,
			read_waker,
			buffer,
			state
        })
	}
}

impl futures::sink::Sink<&[u8]> for WSStream {
	type Error = Error;
    fn poll_ready(
        self: Pin<&mut Self>, 
        _cx: &mut Context<'_>
    ) -> Poll<Result<(), Self::Error>> {
		if *(self.state.borrow()) == State::Closed {
			Poll::Ready(Err(Error::CloseSocket))
		} else {
			Poll::Ready(Ok(()))
		}
	}
    fn start_send(self: Pin<&mut Self>, item: &[u8]) -> Result<(), Self::Error> {
		if *(self.state.borrow()) == State::Closed {
			Err(Error::CloseSocket)
		} else {
			self.ws.send_with_u8_array(item).map_err(|_| Error::Any)
		}
	}
    fn poll_flush(
        self: Pin<&mut Self>, 
        _cx: &mut Context<'_>
    ) -> Poll<Result<(), Self::Error>> {
		if *(self.state.borrow()) == State::Closed {
			Poll::Ready(Err(Error::CloseSocket))
		} else {
			Poll::Ready(Ok(()))
		}
	}

    fn poll_close(
        self: Pin<&mut Self>, 
        _cx: &mut Context<'_>
    ) -> Poll<Result<(), Self::Error>> {
		self.ws.set_onclose(None);
		self.on_close_cb.as_ref().unchecked_ref::<js_sys::Function>().call0(&wasm_bindgen::JsValue::NULL).ok();
		self.ws.close().ok();
		Poll::Ready(Ok(()))
	}
}

impl futures::stream::Stream for WSStream {
	type Item = Bytes;
	fn poll_next(
        self: Pin<&mut Self>, 
        cx: &mut Context<'_>
    ) -> Poll<Option<Self::Item>> {
		let mut internal_buf = self.buffer.borrow_mut();
		let in_len = internal_buf.len();
		if *(self.state.borrow()) == State::Closed {
			Poll::Ready(None)
		} else if in_len == 0 {
			self.read_waker.replace(Some(cx.waker().clone()));
			Poll::Pending
		} else {
			let ret = std::mem::replace(&mut *internal_buf, BytesMut::new());
			Poll::Ready(Some(ret.freeze()))			
		}
	}
}

// TODO: headless browser tests
/*
#[wasm_bindgen(start)]
pub async fn start() -> Result<(), JsValue> {
	console_log!("It started");
	let ws = WSStream::connect(&"ws://localhost:8080/foo").await;
	console_log!("socket return: {:?}", ws);
	if let Ok(ws) = ws {
		console_log!("Sendong test123...");
		let (mut sender, mut receiver) = ws.split();
		sender.send(b"test123").await.ok();
		console_log!("Starting to wait for the read...");
		while let Some(data) = receiver.next().await {
			console_log!("from userspace: {:?}", data);
			// console_log!("clonsing...");
			// sender.close().await.ok();
			// console_log!("clonsing ok");
		}
		console_log!("Socket stream is terminated");
		// ws.close();
	} else {
		console_log!("this is an error");
	}
	Ok(())
}
*/
// #[cfg(test)]
// mod tests {
//     #[test]
//     fn it_works() {
//         assert_eq!(2 + 2, 4);
//     }
// }

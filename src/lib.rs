#![no_std]

//! # web-ws
//!
//! WASM library to use JavaScript's WebSockets easly.
//!
//! ## Usage
//!
//! ```rust
//! use web_ws::WSStream;
//!
//! #[wasm_bindgen(start)]
//! pub async fn start() -> Result<(), JsValue> {
//! 	let ws = WSStream::connect(&"ws://localhost:8080/foo").await;
//! 	if let Ok(ws) = ws {
//! 		let (mut sender, mut receiver) = ws.split();
//! 		sender.send(b"test123").await.ok();
//! 		while let Some(data) = receiver.next().await {
//! 			console_log!("{:?}", data);
//! 			// if ... { sender.close(); }
//! 		}
//! 	}
//! 	Ok(())
//! }
//! ```

use bytes::{Bytes, BytesMut};
use core::cell::RefCell;
use core::pin::Pin;
use futures::channel::mpsc::{Sender as BoundedSender, UnboundedSender};

use futures::channel::oneshot;
use futures::future::{self, Either};
use futures::task::{Context, Poll, Waker};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{ErrorEvent, MessageEvent, WebSocket};

extern crate alloc;
use alloc::prelude::v1::Box;
use alloc::rc::Rc;
use alloc::string::{String, ToString};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}

macro_rules! console_log {
    ($($t:tt)*) => (log(&format_args!($($t)*).to_string()))
}

#[derive(Debug, PartialEq, Clone)]
enum State {
    Connected,
    Closed,
    Errored(ErrorEvent),
}

#[derive(Debug)]
pub enum Error {
    Any,
    CloseSocket,
    Js(JsValue),
    NotCompatible,
}

#[derive(Debug, Clone)]
pub enum ChannelSender {
    Unbounded(UnboundedSender<Bytes>),
    Bounded(BoundedSender<Bytes>),
}

#[derive(Debug, Clone)]
pub enum Sender {
    Buffer(Rc<RefCell<BytesMut>>),
    Sender(ChannelSender),
}

impl Sender {
    pub fn send(&mut self, data: Bytes) -> Result<(), String> {
        match self {
            Sender::Buffer(buffer) => {
                buffer.borrow_mut().extend_from_slice(&data);
                Ok(())
            }
            Sender::Sender(ChannelSender::Bounded(bounded)) => {
                bounded.try_send(data).map_err(|e| alloc::format!("{}", e))
            }
            Sender::Sender(ChannelSender::Unbounded(unbounded)) => unbounded
                .unbounded_send(data)
                .map_err(|e| alloc::format!("{}", e)),
        }
    }
}

impl From<UnboundedSender<Bytes>> for Sender {
    fn from(sender: UnboundedSender<Bytes>) -> Self {
        Sender::Sender(ChannelSender::Unbounded(sender))
    }
}

impl From<BoundedSender<Bytes>> for Sender {
    fn from(sender: BoundedSender<Bytes>) -> Self {
        Sender::Sender(ChannelSender::Bounded(sender))
    }
}

impl From<()> for Sender {
    fn from(_sender: ()) -> Self {
        Sender::Buffer(Rc::new(RefCell::new(BytesMut::new())))
    }
}

#[derive(Debug)]
pub struct WSStream {
    ws: WebSocket,
    on_message_cb: Closure<dyn FnMut(MessageEvent)>,
    on_error_cb: Closure<dyn FnMut(ErrorEvent)>,
    on_close_cb: Closure<dyn FnMut(JsValue)>,
    state: Rc<RefCell<State>>,
    read_waker: Rc<RefCell<Option<Waker>>>,
    sender: Sender,
}

impl Drop for WSStream {
    fn drop(&mut self) {
        self.ws.set_onclose(None);
        self.on_close_cb
            .as_ref()
            .unchecked_ref::<js_sys::Function>()
            .call0(&wasm_bindgen::JsValue::NULL)
            .ok();
        self.ws.close().ok();
    }
}

impl WSStream {
    /// Create a new opened WebSocket connection from an address.
    ///
    /// Return an error if the connection cannot be opened.

    pub async fn connect(url: &str, receiver: Option<Sender>) -> Result<WSStream, Error> {
        let ws = WebSocket::new(url).map_err(|e| Error::Js(e))?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        // Value shared with the callbacks and the struct
        let state = Rc::new(RefCell::new(State::Connected));
        let sender = if let Some(receiver) = receiver {
            receiver
        } else {
            Sender::from(())
        };
        let waker: Rc<RefCell<Option<Waker>>> = Rc::new(RefCell::new(None));

        // On Message
        let mut sender_cl = sender.clone();
        let waker_cl = waker.clone();
        let on_message_cb = Closure::wrap(Box::new(move |e: MessageEvent| {
            let data = if let Ok(abuf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = js_sys::Uint8Array::new(&abuf);
                Bytes::from(array.to_vec())
            } else if let Ok(txt) = e.data().dyn_into::<js_sys::JsString>() {
                Bytes::from(String::from(&txt))
            } else {
                console_log!("Unknow incomming data");
                return;
            };
            sender_cl.send(data).map_err(|e| console_log!("{}", e)).ok();
            waker_cl.borrow().as_ref().map(|waker| waker.wake_by_ref());
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(on_message_cb.as_ref().unchecked_ref()));

        // On close
        let state_cl = state.clone();
        let waker_cl = waker.clone();
        let on_close_cb = Closure::wrap(Box::new(move |_| {
            *state_cl.borrow_mut() = State::Closed;
            waker_cl.borrow().as_ref().map(|waker| waker.wake_by_ref());
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onclose(Some(on_close_cb.as_ref().unchecked_ref()));

        // On Connect Error
        let (send_error_connect, recv_error_connect) = oneshot::channel::<ErrorEvent>();
        let mut send_error_connect = Some(send_error_connect);
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
            Either::Left((_, _)) => { /* Received something on recv_ok_connect */ }
            Either::Right((e, _)) => {
                ws.set_onclose(None);
                return Err(Error::Js(e.unwrap().error())); // Err if sender droped, not the case here
            }
        }
        ws.set_onopen(None);

        // On error
        let state_cl = state.clone();
        let on_error_cb = Closure::wrap(Box::new(move |e: ErrorEvent| {
            *state_cl.borrow_mut() = State::Errored(e);
        }) as Box<dyn FnMut(ErrorEvent)>);
        ws.set_onerror(Some(on_error_cb.as_ref().unchecked_ref()));

        Ok(WSStream {
            ws,
            on_error_cb,
            on_message_cb,
            on_close_cb,
            read_waker: waker,
            sender,
            state,
        })
    }

    pub fn send_sync(&self, data: &[u8]) -> Result<(), Error> {
        self.ws.send_with_u8_array(data).map_err(Error::Js)
    }
}

impl futures::sink::Sink<Bytes> for WSStream {
    type Error = Error;
    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if *(self.state.borrow()) == State::Closed {
            Poll::Ready(Err(Error::CloseSocket))
        } else {
            Poll::Ready(Ok(()))
        }
    }
    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        if *(self.state.borrow()) == State::Closed {
            Err(Error::CloseSocket)
        } else {
            self.ws.send_with_u8_array(&item).map_err(|_| Error::Any)
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if *(self.state.borrow()) == State::Closed {
            Poll::Ready(Err(Error::CloseSocket))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.ws.set_onclose(None);
        self.on_close_cb
            .as_ref()
            .unchecked_ref::<js_sys::Function>()
            .call0(&wasm_bindgen::JsValue::NULL)
            .ok();
        self.ws.close().ok();
        Poll::Ready(Ok(()))
    }
}

impl futures::stream::Stream for WSStream {
    type Item = Result<Bytes, Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &self.sender {
            Sender::Buffer(buffer) => {
                let mut internal_buf = buffer.borrow_mut();
                let in_len = internal_buf.len();
                if *(self.state.borrow()) == State::Closed {
                    Poll::Ready(None)
                } else if in_len == 0 {
                    self.read_waker.replace(Some(cx.waker().clone()));
                    Poll::Pending
                } else {
                    let ret = core::mem::replace(&mut *internal_buf, BytesMut::new());
                    Poll::Ready(Some(Ok(ret.freeze())))
                }
            }
            _ => Poll::Ready(Some(Err(Error::NotCompatible))),
        }
    }
}

// TODO: headless browser tests
/*
   use futures::StreamExt;
   use futures::SinkExt;
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
sender.close().await.ok();
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

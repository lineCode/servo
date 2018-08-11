/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use cookie::Cookie;
use hosts::replace_host;
use http_loader::HttpState;
use hyper::header::{Headers, Host, SetCookie};
use ipc_channel::ipc::{IpcReceiver, IpcSender};
use net_traits::{CookieSource, MessageData};
use net_traits::{WebSocketDomAction, WebSocketNetworkEvent};
use net_traits::request::{RequestInit, RequestMode};
use servo_url::ServoUrl;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use url::Url;
use ws::{CloseCode, Factory, Handler, Handshake, Message, Request, Response as WsResponse, Sender, WebSocket};
use ws::{Error as WebSocketError, ErrorKind as WebSocketErrorKind, Result as WebSocketResult};

/// A client for connecting to a websocket server
#[derive(Clone)]
struct Client<'a> {
    origin: &'a str,
    host: &'a Host,
    protocols: &'a [String],
    http_state: &'a Arc<HttpState>,
    resource_url: &'a ServoUrl,
    event_sender: &'a IpcSender<WebSocketNetworkEvent>,
    protocol_in_use: Option<String>,
}

impl<'a> Factory for Client<'a> {
    type Handler = Self;

    fn connection_made(&mut self, _: Sender) -> Self::Handler {
        self.clone()
    }

    fn connection_lost(&mut self, _: Self::Handler) {
        let _ = self.event_sender.send(WebSocketNetworkEvent::Fail);
    }
}


impl<'a> Handler for Client<'a> {
    fn build_request(&mut self, url: &Url) -> WebSocketResult<Request> {
        let mut req = Request::from_url(url)?;
        req.headers_mut().push(("Origin".to_string(), self.origin.as_bytes().to_owned()));
        req.headers_mut().push(("Host".to_string(), format!("{}", self.host).as_bytes().to_owned()));

        for protocol in self.protocols {
            req.add_protocol(protocol);
        };

        let mut cookie_jar = self.http_state.cookie_jar.write().unwrap();
        if let Some(cookie_list) = cookie_jar.cookies_for_url(self.resource_url, CookieSource::HTTP) {
            req.headers_mut().push(("Cookie".into(), cookie_list.as_bytes().to_owned()))
        }

        Ok(req)
    }

    fn on_open(&mut self, shake: Handshake) -> WebSocketResult<()> {
        let mut headers = Headers::new();
        for &(ref name, ref value) in shake.response.headers().iter() {
            headers.set_raw(name.clone(), vec![value.clone()]);
        }

         if let Some(cookies) = headers.get::<SetCookie>() {
            let mut jar = self.http_state.cookie_jar.write().unwrap();
            for cookie in &**cookies {
                if let Some(cookie) =
                    Cookie::from_cookie_string(cookie.clone(), self.resource_url, CookieSource::HTTP)
                {
                    jar.push(cookie, self.resource_url, CookieSource::HTTP);
                }
            }
        }

        let _ = self.event_sender.send(
            WebSocketNetworkEvent::ConnectionEstablished { protocol_in_use: self.protocol_in_use.clone() });
        Ok(())
    }

     fn on_message(&mut self, message: Message) -> WebSocketResult<()> {
        let message = match message {
            Message::Text(message) => MessageData::Text(message),
            Message::Binary(message) => MessageData::Binary(message),
        };
        let _ = self.event_sender.send(WebSocketNetworkEvent::MessageReceived(message));

        Ok(())
    }


    fn on_error(&mut self, err: WebSocketError) {
        debug!("Error in WebSocket communication: {:?}", err);
        let _ = self.event_sender.send(WebSocketNetworkEvent::Fail);
    }


    fn on_response(&mut self, res: &WsResponse) -> WebSocketResult<()> {
        let protocol_in_use = res.protocol()?;
        if let Some(protocol_name) = protocol_in_use {
            let protocol_name = protocol_name.to_lowercase();
            if !self.protocols.is_empty() && !self.protocols.iter().any(|p| protocol_name == (*p).to_lowercase()) {
                let error = WebSocketError::new(WebSocketErrorKind::Protocol,
                                                "Protocol in Use not in client-supplied protocol list");
                return Err(error);
            }
            self.protocol_in_use = Some(protocol_name);
        }
        Ok(())
    }

    fn on_close(&mut self, code: CloseCode, reason: &str) {
        debug!("Connection closing due to ({:?}) {}", code, reason);
        let _ = self.event_sender.send(WebSocketNetworkEvent::Close(Some(code.into()), reason.to_owned()));
    }
}

pub fn init(
    req_init: RequestInit,
    resource_event_sender: IpcSender<WebSocketNetworkEvent>,
    dom_action_receiver: IpcReceiver<WebSocketDomAction>,
    http_state: Arc<HttpState>
) {
    thread::Builder::new().name(format!("WebSocket connection to {}", req_init.url)).spawn(move || {
        let protocols = match req_init.mode {
            RequestMode::WebSocket { protocols } => protocols.clone(),
            _ => panic!("Received a RequestInit with a non-websocket mode in websocket_loader"),
        };
        let net_url = replace_host(&req_init.url.as_str());

        let host = Host {
            hostname: req_init.url.host_str().unwrap().to_owned(),
            port: req_init.url.port_or_known_default(),
        };

        let client = Client {
            origin: &req_init.origin.ascii_serialization(),
            host: &host,
            protocols: &protocols,
            http_state: &http_state,
            resource_url: &req_init.url,
            event_sender: &resource_event_sender,
            protocol_in_use: None,
        };
        let mut ws = WebSocket::new(client).unwrap();

        if let Err(e) = ws.connect(net_url.parse().unwrap()) {
            debug!("Failed to establish a WebSocket connection: {:?}", e);
            return;
        };

        let ws_sender = ws.broadcaster();
        let initiated_close = Arc::new(AtomicBool::new(false));

        thread::spawn(move || {
            while let Ok(dom_action) = dom_action_receiver.recv() {
                match dom_action {
                    WebSocketDomAction::SendMessage(MessageData::Text(data)) => {
                        ws_sender.send(Message::text(data)).unwrap();
                    },
                    WebSocketDomAction::SendMessage(MessageData::Binary(data)) => {
                        ws_sender.send(Message::binary(data)).unwrap();
                    },
                    WebSocketDomAction::Close(code, reason) => {
                        if !initiated_close.fetch_or(true, Ordering::SeqCst) {
                            match code {
                                Some(code) => {
                                    ws_sender.close_with_reason(code.into(), reason.unwrap_or("".to_owned())).unwrap()
                                },
                                None => ws_sender.close(CloseCode::Status).unwrap(),
                            };
                        }
                    },
                }
            }
        });

        if let Err(e) = ws.run() {
            debug!("Failed to run WebSocket: {:?}", e);
            let _ = resource_event_sender.send(WebSocketNetworkEvent::Fail);
        };
    }).expect("Thread spawning failed");
}


//! Multiplex many MCP clients onto one server process.
//!
//! The router is pure: it takes JSON-RPC messages from clients or from the server and returns
//! where each resulting message goes. The daemon does the I/O.
//!
//! What it takes care of:
//! - request ids: every client numbers its requests from 1, so ids are rewritten to a single
//!   sequence on the way in and restored on the way out;
//! - the handshake: the server is initialized once, later clients get the cached result and
//!   their `notifications/initialized` is dropped;
//! - progress tokens and cancellations, which name ids and tokens of one particular client;
//! - requests from the server (sampling, roots, elicitation), which go to the client that was
//!   active most recently;
//! - notifications from the server, which every client gets.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Value, json};

pub type ClientId = u64;

#[derive(Debug, PartialEq)]
pub enum Out {
    Server(Value),
    Client(ClientId, Value),
}

struct Pending {
    client: ClientId,
    id: Value,
    progress: Option<String>,
    is_init: bool,
}

enum Init {
    NotStarted,
    InFlight { waiters: Vec<(ClientId, Value)> },
    Done(Value),
}

pub struct Router {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    init: Init,
    initialized_sent: bool,
    /// Rewritten progress token → the client that asked and its own token.
    progress: HashMap<String, (ClientId, Value)>,
    /// Server request id (as JSON text) → the client it was sent to.
    server_requests: HashMap<String, ClientId>,
    clients: BTreeSet<ClientId>,
    last_active: Option<ClientId>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn has_id(msg: &Value) -> bool {
    msg.get("id").is_some_and(|id| !id.is_null())
}

impl Router {
    pub fn new() -> Self {
        Router {
            next_id: 1,
            pending: HashMap::new(),
            init: Init::NotStarted,
            initialized_sent: false,
            progress: HashMap::new(),
            server_requests: HashMap::new(),
            clients: BTreeSet::new(),
            last_active: None,
        }
    }

    #[cfg(test)]
    pub fn clients(&self) -> usize {
        self.clients.len()
    }

    pub fn join(&mut self, client: ClientId) {
        self.clients.insert(client);
    }

    fn alloc(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn on_client(&mut self, client: ClientId, msg: Value) -> Vec<Out> {
        if let Value::Array(batch) = msg {
            return batch
                .into_iter()
                .flat_map(|m| self.on_client(client, m))
                .collect();
        }
        let method = msg.get("method").and_then(Value::as_str).map(String::from);
        match (method, has_id(&msg)) {
            (Some(method), true) => self.client_request(client, &method, msg),
            (Some(method), false) => self.client_notification(client, &method, msg),
            (None, true) => self.client_response(msg),
            (None, false) => Vec::new(),
        }
    }

    fn client_request(&mut self, client: ClientId, method: &str, mut msg: Value) -> Vec<Out> {
        self.last_active = Some(client);
        let id = msg["id"].clone();
        match method {
            "ping" => return vec![Out::Client(client, response(&id, json!({})))],
            "initialize" => match &mut self.init {
                Init::Done(result) => {
                    return vec![Out::Client(client, response(&id, result.clone()))];
                }
                Init::InFlight { waiters } => {
                    waiters.push((client, id));
                    return Vec::new();
                }
                Init::NotStarted => {
                    self.init = Init::InFlight {
                        waiters: Vec::new(),
                    };
                }
            },
            _ => {}
        }

        let gid = self.alloc();
        let mut progress = None;
        if let Some(token) = msg.pointer_mut("/params/_meta/progressToken") {
            let ours = format!("mcpherd-{gid}");
            self.progress.insert(
                ours.clone(),
                (client, std::mem::replace(token, json!(ours))),
            );
            progress = Some(format!("mcpherd-{gid}"));
        }
        msg["id"] = json!(gid);
        self.pending.insert(
            gid,
            Pending {
                client,
                id,
                progress,
                is_init: method == "initialize",
            },
        );
        vec![Out::Server(msg)]
    }

    fn client_notification(&mut self, client: ClientId, method: &str, mut msg: Value) -> Vec<Out> {
        match method {
            "notifications/initialized" => {
                if self.initialized_sent || !matches!(self.init, Init::Done(_)) {
                    return Vec::new();
                }
                self.initialized_sent = true;
            }
            "notifications/cancelled" => {
                let Some(theirs) = msg.pointer("/params/requestId").cloned() else {
                    return Vec::new();
                };
                let found = self
                    .pending
                    .iter()
                    .find(|(_, p)| p.client == client && p.id == theirs && !p.is_init)
                    .map(|(gid, _)| *gid);
                let Some(gid) = found else {
                    return Vec::new();
                };
                msg["params"]["requestId"] = json!(gid);
            }
            _ => {}
        }
        vec![Out::Server(msg)]
    }

    fn client_response(&mut self, msg: Value) -> Vec<Out> {
        // An answer to a request the server sent; its id was never rewritten.
        match self.server_requests.remove(&msg["id"].to_string()) {
            Some(_) => vec![Out::Server(msg)],
            None => Vec::new(),
        }
    }

    pub fn on_server(&mut self, msg: Value) -> Vec<Out> {
        if let Value::Array(batch) = msg {
            return batch.into_iter().flat_map(|m| self.on_server(m)).collect();
        }
        let method = msg.get("method").and_then(Value::as_str).map(String::from);
        match (method, has_id(&msg)) {
            (None, true) => self.server_response(msg),
            (Some(method), true) => self.server_request(&method, msg),
            (Some(method), false) => self.server_notification(&method, msg),
            (None, false) => Vec::new(),
        }
    }

    fn server_response(&mut self, mut msg: Value) -> Vec<Out> {
        let Some(p) = msg["id"].as_u64().and_then(|gid| self.pending.remove(&gid)) else {
            return Vec::new();
        };
        if let Some(token) = &p.progress {
            self.progress.remove(token);
        }

        if p.is_init {
            let waiters = match std::mem::replace(&mut self.init, Init::NotStarted) {
                Init::InFlight { waiters } => waiters,
                _ => Vec::new(),
            };
            if let Some(result) = msg.get("result") {
                self.init = Init::Done(result.clone());
            }
            return std::iter::once((p.client, p.id))
                .chain(waiters)
                .filter(|(c, _)| self.clients.contains(c))
                .map(|(c, id)| {
                    let mut m = msg.clone();
                    m["id"] = id;
                    Out::Client(c, m)
                })
                .collect();
        }

        if !self.clients.contains(&p.client) {
            return Vec::new();
        }
        msg["id"] = p.id;
        vec![Out::Client(p.client, msg)]
    }

    fn server_request(&mut self, method: &str, msg: Value) -> Vec<Out> {
        let id = msg["id"].clone();
        if method == "ping" {
            return vec![Out::Server(response(&id, json!({})))];
        }
        let target = self
            .last_active
            .filter(|c| self.clients.contains(c))
            .or_else(|| self.clients.iter().next_back().copied());
        match target {
            Some(c) => {
                self.server_requests.insert(id.to_string(), c);
                vec![Out::Client(c, msg)]
            }
            None => vec![Out::Server(error(&id, -32603, "no client is connected"))],
        }
    }

    fn server_notification(&mut self, method: &str, mut msg: Value) -> Vec<Out> {
        match method {
            "notifications/progress" => {
                let token = msg.pointer("/params/progressToken").and_then(Value::as_str);
                let Some((client, theirs)) = token.and_then(|t| self.progress.get(t)).cloned()
                else {
                    return Vec::new();
                };
                msg["params"]["progressToken"] = theirs;
                vec![Out::Client(client, msg)]
            }
            "notifications/cancelled" => {
                let key = msg
                    .pointer("/params/requestId")
                    .map(Value::to_string)
                    .unwrap_or_default();
                match self.server_requests.remove(&key) {
                    Some(c) => vec![Out::Client(c, msg)],
                    None => Vec::new(),
                }
            }
            _ => self
                .clients
                .iter()
                .map(|c| Out::Client(*c, msg.clone()))
                .collect(),
        }
    }

    /// A client went away: cancel what it was waiting for and answer what it owed the server.
    pub fn leave(&mut self, client: ClientId) -> Vec<Out> {
        self.clients.remove(&client);
        if self.last_active == Some(client) {
            self.last_active = None;
        }
        let mut out = Vec::new();

        let mine: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| p.client == client && !p.is_init)
            .map(|(gid, _)| *gid)
            .collect();
        for gid in mine {
            if let Some(p) = self.pending.remove(&gid)
                && let Some(token) = p.progress
            {
                self.progress.remove(&token);
            }
            out.push(Out::Server(json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": gid, "reason": "client disconnected"}
            })));
        }

        if let Init::InFlight { waiters } = &mut self.init {
            waiters.retain(|(c, _)| *c != client);
        }

        let owed: Vec<String> = self
            .server_requests
            .iter()
            .filter(|(_, c)| **c == client)
            .map(|(id, _)| id.clone())
            .collect();
        for key in owed {
            self.server_requests.remove(&key);
            let id: Value = serde_json::from_str(&key).unwrap_or(Value::Null);
            out.push(Out::Server(error(&id, -32603, "client disconnected")));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: Value, method: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {}})
    }

    fn server(out: &[Out]) -> Vec<&Value> {
        out.iter()
            .filter_map(|o| match o {
                Out::Server(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    fn to(out: &[Out], client: ClientId) -> Vec<&Value> {
        out.iter()
            .filter_map(|o| match o {
                Out::Client(c, v) if *c == client => Some(v),
                _ => None,
            })
            .collect()
    }

    fn init_result() -> Value {
        json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "s"}})
    }

    /// Two clients connected, the server initialized through client 1.
    fn ready() -> Router {
        let mut r = Router::new();
        r.join(1);
        r.join(2);
        let out = r.on_client(1, req(json!(0), "initialize"));
        let gid = server(&out)[0]["id"].clone();
        r.on_server(json!({"jsonrpc": "2.0", "id": gid, "result": init_result()}));
        r.on_client(
            1,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        r
    }

    #[test]
    fn initializes_once_and_caches_the_result() {
        let mut r = Router::new();
        r.join(1);
        r.join(2);

        let out = r.on_client(1, req(json!(0), "initialize"));
        assert_eq!(server(&out).len(), 1);
        // A second client asks while the first handshake is still in flight.
        assert!(r.on_client(2, req(json!("a"), "initialize")).is_empty());

        let gid = server(&out)[0]["id"].clone();
        let out = r.on_server(json!({"jsonrpc": "2.0", "id": gid, "result": init_result()}));
        assert_eq!(to(&out, 1)[0]["id"], json!(0));
        assert_eq!(to(&out, 2)[0]["id"], json!("a"));
        assert_eq!(to(&out, 2)[0]["result"], init_result());

        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(server(&r.on_client(1, initialized.clone())).len(), 1);
        assert!(r.on_client(2, initialized).is_empty());

        // A late client is answered from the cache without touching the server.
        r.join(3);
        let out = r.on_client(3, req(json!(7), "initialize"));
        assert!(server(&out).is_empty());
        assert_eq!(to(&out, 3)[0]["id"], json!(7));
    }

    #[test]
    fn rewrites_colliding_ids() {
        let mut r = ready();
        let a = r.on_client(1, req(json!(1), "tools/call"));
        let b = r.on_client(2, req(json!(1), "tools/call"));
        let (ga, gb) = (server(&a)[0]["id"].clone(), server(&b)[0]["id"].clone());
        assert_ne!(ga, gb);

        let out = r.on_server(json!({"jsonrpc": "2.0", "id": gb, "result": {"n": 2}}));
        assert_eq!(
            to(&out, 2)[0],
            &json!({"jsonrpc": "2.0", "id": 1, "result": {"n": 2}})
        );
        assert!(to(&out, 1).is_empty());

        let out = r.on_server(json!({"jsonrpc": "2.0", "id": ga, "result": {"n": 1}}));
        assert_eq!(to(&out, 1)[0]["result"], json!({"n": 1}));
    }

    #[test]
    fn routes_progress_and_cancellation() {
        let mut r = ready();
        let mut call = req(json!(5), "tools/call");
        call["params"]["_meta"] = json!({"progressToken": 5});
        let out = r.on_client(2, call);
        let sent = server(&out)[0].clone();
        let token = sent["params"]["_meta"]["progressToken"].clone();
        assert_ne!(token, json!(5));

        let out = r.on_server(json!({"jsonrpc": "2.0", "method": "notifications/progress",
            "params": {"progressToken": token, "progress": 1}}));
        assert_eq!(to(&out, 2)[0]["params"]["progressToken"], json!(5));
        assert!(to(&out, 1).is_empty());

        let out = r.on_client(
            2,
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 5}}),
        );
        assert_eq!(server(&out)[0]["params"]["requestId"], sent["id"]);
        // Client 1 can't cancel client 2's request.
        let out = r.on_client(
            1,
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 5}}),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn server_requests_go_to_the_active_client() {
        let mut r = ready();
        r.on_client(2, req(json!(1), "tools/call"));
        let out = r.on_server(req(json!(99), "sampling/createMessage"));
        assert_eq!(to(&out, 2)[0]["id"], json!(99));

        let out = r.on_client(2, json!({"jsonrpc": "2.0", "id": 99, "result": {}}));
        assert_eq!(server(&out)[0]["id"], json!(99));
        // Answering twice does nothing.
        assert!(
            r.on_client(2, json!({"jsonrpc": "2.0", "id": 99, "result": {}}))
                .is_empty()
        );

        let out = r.on_server(req(json!(3), "ping"));
        assert_eq!(server(&out)[0], &response(&json!(3), json!({})));
    }

    #[test]
    fn broadcasts_server_notifications() {
        let mut r = ready();
        let out =
            r.on_server(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
        assert_eq!(to(&out, 1).len(), 1);
        assert_eq!(to(&out, 2).len(), 1);
    }

    #[test]
    fn cleans_up_after_a_client_leaves() {
        let mut r = ready();
        let out = r.on_client(2, req(json!(1), "tools/call"));
        let gid = server(&out)[0]["id"].clone();
        r.on_client(2, req(json!(2), "tools/list"));
        r.on_server(req(json!("s1"), "roots/list"));

        let out = r.leave(2);
        let sent = server(&out);
        let cancelled = sent
            .iter()
            .filter(|m| m["method"] == "notifications/cancelled")
            .count();
        assert_eq!(cancelled, 2);
        assert!(
            sent.iter()
                .any(|m| m["id"] == json!("s1") && m.get("error").is_some())
        );

        // A late answer for the departed client goes nowhere.
        assert!(
            r.on_server(json!({"jsonrpc": "2.0", "id": gid, "result": {}}))
                .is_empty()
        );
        assert_eq!(r.clients(), 1);
    }

    #[test]
    fn splits_batches() {
        let mut r = ready();
        let out = r.on_client(
            1,
            json!([req(json!(1), "tools/list"), req(json!(2), "ping")]),
        );
        assert_eq!(server(&out).len(), 1);
        assert_eq!(to(&out, 1)[0]["id"], json!(2));
    }
}

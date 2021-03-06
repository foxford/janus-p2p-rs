#[macro_use]
extern crate janus_plugin as janus;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;

mod messages;

use janus::{JanssonValue, Plugin, PluginCallbacks, PluginMetadata, PluginResult, PluginSession,
            RawJanssonValue, RawPluginResult};
use janus::session::SessionWrapper;
use messages::Response;
use std::collections::HashMap;
use std::error::Error;
use std::os::raw::{c_char, c_int};
use std::sync::{mpsc, Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};

macro_rules! c_str {
    ($lit:expr) => {
        unsafe {
            ::std::ffi::CStr::from_ptr(concat!($lit, "\0").as_ptr() as *const ::std::os::raw::c_char)
        }
    }
}

lazy_static! {
    static ref CHANNEL: Mutex<Option<mpsc::Sender<RawMessage>>> = Mutex::new(None);
    static ref SESSIONS: RwLock<Vec<Box<Arc<Session>>>> = RwLock::new(Vec::new());
    #[derive(Debug)]
    static ref ROOMS: RwLock<HashMap<RoomId, Box<Room>>> = RwLock::new(HashMap::new());
}

static mut GATEWAY: Option<&PluginCallbacks> = None;

#[derive(Debug)]
struct RawMessage {
    session: Weak<Session>,
    transaction: *mut c_char,
    message: Option<JanssonValue>,
    jsep: Option<JanssonValue>,
}
unsafe impl std::marker::Send for RawMessage {}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RoomId(u64);

#[derive(Debug)]
struct Room {
    id: RoomId,
    caller: Option<Weak<Session>>,
    callee: Option<Weak<Session>>,
}

impl Room {
    fn new(id: RoomId) -> Room {
        Room {
            id,
            callee: None,
            caller: None,
        }
    }

    fn is_new(id: RoomId) -> bool {
        let rooms = ROOMS.read().unwrap();
        !rooms.contains_key(&id)
    }

    fn is_empty(&self) -> bool {
        self.caller.is_none() && self.callee.is_none()
    }

    fn create(this: Room) {
        let mut rooms = ROOMS.write().unwrap();
        rooms.insert(this.id, Box::new(this));
    }

    fn get_mut(rooms: &mut HashMap<RoomId, Box<Room>>, id: RoomId) -> &mut Box<Room> {
        rooms.get_mut(&id).unwrap()
    }

    fn all() -> RwLockReadGuard<'static, HashMap<RoomId, Box<Room>>> {
        ROOMS.read().expect("Cannot lock ROOMS for read")
    }

    fn all_mut() -> RwLockWriteGuard<'static, HashMap<RoomId, Box<Room>>> {
        ROOMS.write().expect("Cannot lock ROOMS for write")
    }

    fn add_member(&mut self, member: RoomMember) {
        match member {
            RoomMember::Callee(ref session) => {
                self.callee = Some(Arc::downgrade(session));
            }
            RoomMember::Caller(ref session) => {
                self.caller = Some(Arc::downgrade(session));
            }
        }
    }
}

#[derive(Debug)]
enum RoomMember {
    Callee(Arc<Session>),
    Caller(Arc<Session>),
}

#[derive(Debug)]
pub struct SessionState {
    room_id: Option<RoomId>,
    initiator: Option<bool>,
}

impl SessionState {
    fn get(session: &Session) -> RwLockReadGuard<SessionState> {
        // deref Arc, deref SessionWrapper
        session.read().expect("Cannot lock session for read")
    }

    fn get_mut(session: &Session) -> RwLockWriteGuard<SessionState> {
        session.write().expect("Cannot lock session for write")
    }

    fn get_room<'a>(&self, rooms: &'a HashMap<RoomId, Box<Room>>) -> &'a Box<Room> {
        rooms
            .get(&self.room_id.expect("Session state has no room id"))
            .unwrap()
    }
}

type Session = SessionWrapper<RwLock<SessionState>>;
type MessageResult = Result<(), Box<Error>>;

extern "C" fn init(callback: *mut PluginCallbacks, _config_path: *const c_char) -> c_int {
    janus_verb!("--> P2P init");

    unsafe {
        let callback = callback.as_ref().unwrap();
        GATEWAY = Some(callback);
    }

    let (tx, rx) = mpsc::channel();
    *(CHANNEL.lock().unwrap()) = Some(tx);

    std::thread::spawn(move || {
        janus_verb!("--> P2P Start handling thread");

        for msg in rx.iter() {
            janus_verb!("Processing message: {:?}", msg);
            handle_message_async(msg).err().map(|e| {
                janus_err!("Error processing message: {}", e);
            });
        }
    });

    0
}

extern "C" fn destroy() {
    janus_verb!("--> P2P destroy");
}

extern "C" fn create_session(handle: *mut PluginSession, error: *mut c_int) {
    let state = SessionState {
        room_id: None,
        initiator: None,
    };
    match unsafe { Session::associate(handle, RwLock::new(state)) } {
        Ok(session) => {
            janus_info!("Initializing P2P session {:?}", session);
            SESSIONS.write().unwrap().push(session);
        }
        Err(e) => {
            janus_err!("{}", e);
            unsafe { *error = -1 };
        }
    }
}

extern "C" fn query_session(_handle: *mut PluginSession) -> *mut RawJanssonValue {
    janus_verb!("--> P2P query_session");
    std::ptr::null_mut()
}

extern "C" fn destroy_session(handle: *mut PluginSession, _error: *mut c_int) {
    janus_verb!("--> P2P destroy_session");

    let session = unsafe { Session::from_ptr(handle) }.unwrap();
    let state = SessionState::get(&session);

    let mut rooms = Room::all_mut();
    let room_id = state.room_id.unwrap();

    let is_empty = {
        let room = Room::get_mut(&mut rooms, room_id);

        SESSIONS
            .write()
            .unwrap()
            .retain(|ref s| s.as_ptr() != handle);

        match state.initiator {
            Some(true) => room.caller = None,
            Some(false) | None => room.callee = None,
        }

        room.is_empty()
    };

    if is_empty {
        janus_verb!("Room #{:?} is empty, removing it.", room_id);
        rooms
            .remove(&room_id)
            .expect("Room must be present in HashMap");
    } else {
        janus_verb!("Room #{:?} is not empty yet.", room_id);
    }
}

extern "C" fn handle_message(
    handle: *mut PluginSession,
    transaction: *mut c_char,
    message: *mut RawJanssonValue,
    jsep: *mut RawJanssonValue,
) -> *mut RawPluginResult {
    janus_verb!("--> P2P handle_message");

    let result = match unsafe { Session::from_ptr(handle) } {
        Ok(ref session) => {
            let message = RawMessage {
                session: Arc::downgrade(session),
                transaction: transaction,
                message: unsafe { JanssonValue::new(message) },
                jsep: unsafe { JanssonValue::new(jsep) },
            };

            let mutex = CHANNEL.lock().unwrap();
            let tx = mutex.as_ref().unwrap();

            janus_verb!("--> P2P sending message to channel");
            tx.send(message).expect("Sending to channel has failed");

            PluginResult::ok_wait(None)
        }
        Err(_) => PluginResult::error(c_str!("No handle associated with session")),
    };
    result.into_raw()
}

extern "C" fn setup_media(_handle: *mut PluginSession) {
    janus_verb!("--> P2P setup_media");
}

extern "C" fn hangup_media(_handle: *mut PluginSession) {
    janus_verb!("--> P2P hangup_media");
}

extern "C" fn incoming_rtp(
    _handle: *mut PluginSession,
    _video: c_int,
    _buf: *mut c_char,
    _len: c_int,
) {
}

extern "C" fn incoming_rtcp(
    _handle: *mut PluginSession,
    _video: c_int,
    _buf: *mut c_char,
    _len: c_int,
) {
}

extern "C" fn incoming_data(_handle: *mut PluginSession, _buf: *mut c_char, _len: c_int) {}

extern "C" fn slow_link(_handle: *mut PluginSession, _uplink: c_int, _video: c_int) {}

fn handle_message_async(msg: RawMessage) -> MessageResult {
    let RawMessage {
        session,
        transaction,
        message,
        ..
    } = msg;

    if let Some(session) = session.upgrade() {
        // TODO: can message be None?
        let message: JanssonValue = message.unwrap();

        match messages::process(&session, message) {
            Ok(resp) => {
                println!("--> Got response: {:?}", resp);
                match resp {
                    Response::Join { peer, mut payload }
                    | Response::Call { peer, mut payload }
                    | Response::Accept { peer, mut payload }
                    | Response::Candidate { peer, mut payload } => match peer.upgrade() {
                        Some(peer) => {
                            {
                                let json_obj = payload.as_object_mut().unwrap();
                                json_obj.entry("ok").or_insert(json!(true));
                            }
                            push_response(&peer, transaction, payload)
                        }
                        None => Err(messages::Error::PeerHasGone)?,
                    },
                }
            }
            Err(err) => {
                janus_err!("Error processing message: {}", err);
                push_response(
                    &session,
                    transaction,
                    json!({ "ok": false, "error": err.description() }),
                )
            }
        }
    } else {
        Ok(janus_warn!("Got a message for destroyed session."))
    }
}

fn push_response(
    peer: &Session,
    transaction: *mut c_char,
    json: serde_json::Value,
) -> MessageResult {
    let mut event = serde_into_jansson(json);

    let push_event_fn = acquire_gateway().push_event;
    Ok(janus::get_result(push_event_fn(
        peer.handle,
        &mut PLUGIN,
        transaction,
        event.as_mut_ref(),
        std::ptr::null_mut(),
    ))?)
}

// TODO: can we pass value by reference?
fn serde_into_jansson(value: serde_json::Value) -> JanssonValue {
    JanssonValue::from_str(&value.to_string(), janus::JanssonDecodingFlags::empty()).unwrap()
}

fn acquire_gateway() -> &'static PluginCallbacks {
    unsafe { GATEWAY }.expect("Gateway is NONE")
}

const PLUGIN: Plugin = build_plugin!(
    PluginMetadata {
        version: 1,
        version_str: c_str!("0.1"),
        description: c_str!("P2P plugin"),
        name: c_str!("P2P plugin"),
        author: c_str!("Aleksey Ivanov"),
        package: c_str!("janus.plugin.p2p"),
    },
    init,
    destroy,
    create_session,
    handle_message,
    setup_media,
    incoming_rtp,
    incoming_rtcp,
    incoming_data,
    slow_link,
    hangup_media,
    destroy_session,
    query_session
);

export_plugin!(&PLUGIN);

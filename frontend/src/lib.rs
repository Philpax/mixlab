#![recursion_limit="512"]

mod component;
mod control;
mod module;
mod service;
mod util;
mod workspace;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use gloo_events::EventListener;
use wasm_bindgen::prelude::*;
use web_sys::{Element, AudioContext, AudioContextOptions, AudioBuffer, AudioBufferSourceNode};
use yew::format::Binary;
use yew::services::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};
use yew::{html, Component, ComponentLink, Html, ShouldRender};

use mixlab_protocol::{ClientMessage, WorkspaceState, ServerMessage, ModuleId, InputId, OutputId, ModuleParams, WindowGeometry, ServerUpdate, Indication, Terminal, ClientOp, ClientSequence};

use util::Sequence;
use workspace::Workspace;



pub struct App {
    link: ComponentLink<Self>,
    websocket: WebSocketTask,
    state: Option<Rc<RefCell<State>>>,
    client_seq: Sequence,
    server_seq: Option<ClientSequence>,
    root_element: Element,
    viewport_width: usize,
    viewport_height: usize,
    audio_ctx: Option<AudioContext>,
    audio_buffer: Option<AudioBuffer>,
    audio_buffer_source_node: Option<AudioBufferSourceNode>,
    audio_buffers: Vec<Vec<f32>>,
    // must be kept alive while app is running:
    _resize_listener: EventListener,
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(audio_ctx) = &self.audio_ctx {
            let _ = audio_ctx.close();
        }
    }
}

#[derive(Debug, Clone)]
pub struct State {
    // modules uses BTreeMap for consistent iteration order:
    modules: BTreeMap<ModuleId, ModuleParams>,
    geometry: HashMap<ModuleId, WindowGeometry>,
    connections: HashMap<InputId, OutputId>,
    indications: HashMap<ModuleId, Indication>,
    inputs: HashMap<ModuleId, Vec<Terminal>>,
    outputs: HashMap<ModuleId, Vec<Terminal>>,
    channels: usize,
    sample_rate: usize,
    samples_per_tick: usize,
}

impl From<WorkspaceState> for State {
    fn from(wstate: WorkspaceState) -> State {
        State {
            modules: wstate.modules.into_iter().collect(),
            geometry: wstate.geometry.into_iter().collect(),
            indications: wstate.indications.into_iter().collect(),
            connections: wstate.connections.into_iter().collect(),
            inputs: wstate.inputs.into_iter().collect(),
            outputs: wstate.outputs.into_iter().collect(),
            channels: wstate.channels,
            sample_rate: wstate.sample_rate,
            samples_per_tick: wstate.samples_per_tick,
        }
    }
}

#[derive(Debug)]
pub enum AppMsg {
    NoOp,
    WindowResize,
    ServerMessage(ServerMessage),
    ClientUpdate(ClientOp),
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = Math)]
    fn random() -> f64;
}

impl Component for App {
    type Message = AppMsg;
    type Properties = ();

    fn create(_: Self::Properties, link: ComponentLink<Self>) -> Self {
        let mut websocket = WebSocketService::new();

        let host = yew::utils::host().expect("expected host from yew");
        let ws_path = format!("ws://{}/session", host);

        let websocket = websocket.connect_binary(&ws_path,
            link.callback(|msg: Binary| {
                match msg {
                    Ok(buff) => {
                        let msg = bincode::deserialize::<ServerMessage>(&buff)
                            .expect("bincode::deserialize");

                        AppMsg::ServerMessage(msg)
                    }
                    Err(e) => {
                        crate::log!("websocket recv error: {:?}", e);
                        AppMsg::NoOp
                    }
                }
            }),
            link.callback(|status: WebSocketStatus| {
                crate::log!("websocket status: {:?}", status);
                AppMsg::NoOp
            }))
        .expect("websocket.connect_binary");

        let window = web_sys::window()
            .expect("window");

        let root_element = window.document()
            .and_then(|doc| doc.document_element())
            .expect("root element");

        let viewport_width = root_element.client_width() as usize;
        let viewport_height = root_element.client_height() as usize;

        let resize_listener = EventListener::new(&window, "resize", {
            let link = link.clone();
            move |_| { link.send_message(AppMsg::WindowResize) }
        });

        App {
            link,
            websocket,
            state: None,
            client_seq: Sequence::new(),
            server_seq: None,
            root_element,
            viewport_width,
            viewport_height,
            audio_buffer: None,
            audio_buffer_source_node: None,
            audio_ctx: None,
            audio_buffers: vec![],
            _resize_listener: resize_listener,
        }
    }

    fn update(&mut self, msg: Self::Message) -> ShouldRender {
        let mut temp_buffer = vec![0.0 as f32; 44100];
        for elem in temp_buffer.iter_mut() {
            *elem = (random() * 2.0 - 1.0) as f32;
        }

        match msg {
            AppMsg::NoOp => false,
            AppMsg::WindowResize => {
                self.viewport_width = self.root_element.client_width() as usize;
                self.viewport_height = self.root_element.client_height() as usize;
                true
            }
            AppMsg::ServerMessage(msg) => {
                match msg {
                    ServerMessage::WorkspaceState(state) => {
                        let sample_rate = state.sample_rate;
                        let samples_per_tick = state.samples_per_tick;
                        let channels = state.channels;

                        crate::log!("{} {} {}", sample_rate, samples_per_tick, channels);

                        self.state = Some(Rc::new(RefCell::new(state.into())));

                        let audio_context = AudioContext::new_with_context_options(
                            AudioContextOptions::new().sample_rate(sample_rate as f32)
                        ).expect("failed to create audio context");
                        let audio_buffer = audio_context.create_buffer(
                            channels as u32,
                            sample_rate as u32,
                            sample_rate as f32
                        ).expect("failed to create audio buffer");

                        let audio_buffer_source_node = audio_context.create_buffer_source().expect("failed to create buffer source");
                        audio_buffer_source_node.set_buffer(Some(&audio_buffer));
                        audio_buffer_source_node.connect_with_audio_node(&audio_context.destination()).expect("failed to connect to destination");
                        audio_buffer_source_node.set_loop(true);
                        let mut temp_buffer = vec![0.0 as f32; 44100];
                        for elem in temp_buffer.iter_mut() {
                            *elem = 0.0;
                        }
                        audio_buffer.copy_to_channel(&mut temp_buffer, 0 as i32).expect("should be able to copy to channel");
                        audio_buffer.copy_to_channel(&mut temp_buffer, 1 as i32).expect("should be able to copy to channel");
                        audio_buffer_source_node.start().expect("failed to start");

                        self.audio_ctx = Some(audio_context);
                        self.audio_buffer = Some(audio_buffer);
                        self.audio_buffer_source_node = Some(audio_buffer_source_node);

                        self.audio_buffers.clear();
                        for _ in 0..channels {
                            self.audio_buffers.push(vec![]);
                        }
                        true
                    }
                    ServerMessage::Sync(seq) => {
                        if Some(seq) <= self.server_seq {
                            panic!("sequence number repeat, desync");
                        }

                        self.server_seq = Some(seq);

                        // re-render if this Sync message caused us to consider
                        // ourselves synced - there may be prior updates
                        // waiting for render
                        self.synced()
                    }
                    ServerMessage::Update(op) => {
                        let mut state = self.state.as_ref()
                            .expect("server should always send a WorkspaceState before a ModelOp")
                            .borrow_mut();

                        match op {
                            ServerUpdate::CreateModule { id, params, geometry, indication, inputs, outputs } => {
                                state.modules.insert(id, params);
                                state.geometry.insert(id, geometry);
                                state.indications.insert(id, indication);
                                state.inputs.insert(id, inputs);
                                state.outputs.insert(id, outputs);

                        self.audio_buffer.as_ref().unwrap().copy_to_channel(&mut temp_buffer, 0 as i32).expect("should be able to copy to channel");
                        self.audio_buffer.as_ref().unwrap().copy_to_channel(&mut temp_buffer, 1 as i32).expect("should be able to copy to channel");
                            }
                            ServerUpdate::UpdateModuleParams(id, new_params) => {
                                if let Some(params) = state.modules.get_mut(&id) {
                                    *params = new_params;
                                }
                            }
                            ServerUpdate::UpdateWindowGeometry(id, new_geometry) => {
                                if let Some(geometry) = state.geometry.get_mut(&id) {
                                    *geometry = new_geometry;
                                }
                            }
                            ServerUpdate::UpdateModuleIndication(id, new_indication) => {
                                if let Some(indication) = state.indications.get_mut(&id) {
                                    *indication = new_indication;
                                }
                            }
                            ServerUpdate::DeleteModule(id) => {
                                state.modules.remove(&id);
                                state.geometry.remove(&id);
                                state.indications.remove(&id);
                                state.inputs.remove(&id);
                                state.outputs.remove(&id);
                            }
                            ServerUpdate::CreateConnection(input, output) => {
                                state.connections.insert(input, output);
                            }
                            ServerUpdate::DeleteConnection(input) => {
                                state.connections.remove(&input);
                            }
                            ServerUpdate::AudioData(data) => {
                                if let Some(web_audio_buffer) = &self.audio_buffer {
                                    for (channel_idx, channel_data) in data.chunks(state.samples_per_tick).enumerate() {
                                        let audio_buffer = &mut self.audio_buffers[channel_idx as usize];
                                        audio_buffer.extend_from_slice(channel_data);
                                        if audio_buffer.len() > state.sample_rate {
                                            audio_buffer.drain(0..(audio_buffer.len() - state.sample_rate));
                                        }

                                        // let mut temp_buffer = audio_buffer.clone();

                                        // crate::log!("updating channel {} with {} samples", channel_idx, channel_data.len());
                                        web_audio_buffer.copy_to_channel(&mut temp_buffer, channel_idx as i32).expect("should be able to copy to channel");
                                    }
                                }
                            }
                        }

                        // only re-render according to server state if all of
                        // our changes have successfully round-tripped
                        self.synced()
                    },
                }
            }
            AppMsg::ClientUpdate(op) => {
                let msg = ClientMessage {
                    sequence: ClientSequence(self.client_seq.next()),
                    op: op,
                };

                let packet = bincode::serialize(&msg)
                    .expect("bincode::serialize");

                let _ = self.websocket.send_binary(Ok(packet));

                false
            }
        }
    }

    fn view(&self) -> Html {
        match &self.state {
            Some(state) => {
                html! {
                    <Workspace
                        app={self.link.clone()}
                        state={state}
                        width={self.viewport_width}
                        height={self.viewport_height}
                    />
                }
            }
            None => html! {}
        }
    }
}

impl App {
    fn synced(&self) -> bool {
        // only re-render according to server state if all of
        // our changes have successfully round-tripped

        let client_seq = self.client_seq.last().map(ClientSequence);

        if self.server_seq == client_seq {
            // server is up to date, re-render
            true
        } else if self.server_seq < client_seq {
            // server is behind, skip render
            false
        } else {
            panic!("server_seq > client_seq, desync")
        }
    }
}

#[wasm_bindgen]
pub fn start() {
    console_error_panic_hook::set_once();

    yew::start_app::<App>();
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn log_str(s: &str);

    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn log_val(v: &wasm_bindgen::JsValue);

    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn warn_str(s: &str);

    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn error_str(s: &str);
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => (crate::log_str(&format_args!($($t)*).to_string()))
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => (crate::warn_str(&format_args!($($t)*).to_string()))
}

#[macro_export]
macro_rules! error {
    ($($t:tt)*) => (crate::error_str(&format_args!($($t)*).to_string()))
}

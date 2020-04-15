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
use wasm_bindgen::JsCast;
use web_sys::{Element, AudioContext, AudioContextOptions, ScriptProcessorNode, AudioProcessingEvent};
use yew::format::Binary;
use yew::services::websocket::{WebSocketService, WebSocketStatus, WebSocketTask};
use yew::{html, Component, ComponentLink, Html, ShouldRender};
use ringbuf::{RingBuffer, Consumer, Producer};

use mixlab_protocol::{ClientMessage, WorkspaceState, ServerMessage, ModuleId, InputId, OutputId, ModuleParams, WindowGeometry, ServerUpdate, Indication, Terminal, ClientOp, ClientSequence, Sample};

use util::Sequence;
use workspace::Workspace;



pub struct App {
    link: ComponentLink<Self>,
    websocket_data: WebSocketTask,
    websocket_audio: WebSocketTask,
    state: Option<Rc<RefCell<State>>>,
    client_seq: Sequence,
    server_seq: Option<ClientSequence>,
    root_element: Element,
    viewport_width: usize,
    viewport_height: usize,
    audio_ctx: Option<AudioContext>,
    audio_buffers: Vec<Producer<f32>>,
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
    AudioData(Vec<Sample>)
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
        let websocket_data = websocket.connect_binary(&ws_path,
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

        let ws_audio_path = format!("ws://{}/session_audio", host);
        let websocket_audio = websocket.connect_binary(&ws_audio_path,
            link.callback(|msg: Binary| {
                match msg {
                    Ok(buff) => {
                        let msg = bincode::deserialize::<Vec<Sample>>(&buff)
                            .expect("bincode::deserialize");

                        AppMsg::AudioData(msg)
                    }
                    Err(e) => {
                        crate::log!("websocket audio recv error: {:?}", e);
                        AppMsg::NoOp
                    }
                }
            }),
            link.callback(|status: WebSocketStatus| {
                crate::log!("websocket status: {:?}", status);
                AppMsg::NoOp
            }))
        .expect("websocket_audio.connect_binary");

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
            websocket_data,
            websocket_audio,
            state: None,
            client_seq: Sequence::new(),
            server_seq: None,
            root_element,
            viewport_width,
            viewport_height,
            audio_ctx: None,
            audio_buffers: vec![],
            _resize_listener: resize_listener,
        }
    }

    fn update(&mut self, msg: Self::Message) -> ShouldRender {
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

                        self.audio_buffers.clear();

                        let mut ring_buffer_receivers = vec![];
                        for _ in 0..channels {
                            let ring_buffer = RingBuffer::<f32>::new(131072);
                            let (prod, cons) = ring_buffer.split();

                            self.audio_buffers.push(prod);
                            ring_buffer_receivers.push(cons);
                        }

                        let audio_context = AudioContext::new_with_context_options(
                            AudioContextOptions::new().sample_rate(sample_rate as f32)
                        ).expect("failed to create audio context");
                        let spn = audio_context.create_script_processor_with_buffer_size(256).expect("failed to create script processor");

                        // create callback
                        let audioprocess_callback = Closure::wrap(Box::new(move |e: AudioProcessingEvent| {
                            let output_buffer = e.output_buffer().expect("failed to get output buffer");

                            for channel in 0..output_buffer.number_of_channels() {
                                let mut data: Vec<f32> = vec![0.0; output_buffer.length() as usize];

                                let receiver = &mut ring_buffer_receivers[channel as usize];
                                // if receiver.len() > data.len() {
                                    receiver.pop_slice(&mut data);
                                // }

                                output_buffer.copy_to_channel(&mut data, channel as i32).expect("failed to copy channel data");
                            }
                        }) as Box<dyn FnMut(AudioProcessingEvent)>);
                        spn.set_onaudioprocess(Some(audioprocess_callback.as_ref().unchecked_ref()));
                        audioprocess_callback.forget();

                        spn.connect_with_audio_node(&audio_context.destination()).expect("failed to connect audio node");

                        self.audio_ctx = Some(audio_context);

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
                        }

                        // only re-render according to server state if all of
                        // our changes have successfully round-tripped
                        self.synced()
                    },
                }
            }
            AppMsg::AudioData(data) => {
                let state = self.state.as_ref();
                if let Some(state) = state {
                    let samples_per_tick = state.borrow().samples_per_tick;
                    for (channel_idx, channel_data) in data.chunks(samples_per_tick).enumerate() {
                        self.audio_buffers[channel_idx as usize].push_slice(channel_data);
                    }
                }

                false
            }
            AppMsg::ClientUpdate(op) => {
                let msg = ClientMessage {
                    sequence: ClientSequence(self.client_seq.next()),
                    op: op,
                };

                let packet = bincode::serialize(&msg)
                    .expect("bincode::serialize");

                let _ = self.websocket_data.send_binary(Ok(packet));

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

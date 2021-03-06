use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::f32;
use std::num::NonZeroUsize;
use std::sync::mpsc::{self, SyncSender, Receiver, RecvTimeoutError, TrySendError};
use std::thread;
use std::time::{Instant, Duration};

use tokio::sync::{oneshot, broadcast};

use mixlab_protocol::{ModuleId, InputId, OutputId, ClientMessage, TerminalId, WorkspaceState, WindowGeometry, ServerUpdate, Indication, LineType, ClientSequence, ClientOp};

use crate::module::Module;
use crate::util::Sequence;

pub type Sample = f32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionId(NonZeroUsize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// NOTE! This is not Ord because log positions with different session IDs have
// no relative ordering
pub struct OpClock(pub SessionId, pub ClientSequence);

impl PartialOrd for OpClock {
    fn partial_cmp(&self, other: &OpClock) -> Option<Ordering> {
        if self.0 == other.0 {
            Some(self.1.cmp(&other.1))
        } else {
            None
        }
    }
}

pub const CHANNELS: usize = 2;
pub const SAMPLE_RATE: usize = 44100;
const TICKS_PER_SECOND: usize = 100;
const SAMPLES_PER_TICK: usize = SAMPLE_RATE / TICKS_PER_SECOND;

pub static ZERO_BUFFER_STEREO: [Sample; SAMPLES_PER_TICK * CHANNELS] = [0.0; SAMPLES_PER_TICK * CHANNELS];

pub static ZERO_BUFFER_MONO: [Sample; SAMPLES_PER_TICK] = [0.0; SAMPLES_PER_TICK];
pub static ONE_BUFFER_MONO: [Sample; SAMPLES_PER_TICK] = [1.0; SAMPLES_PER_TICK];

pub enum EngineMessage {
    ConnectSession(oneshot::Sender<(SessionId, WorkspaceState, EngineOps)>),
    ClientMessage(SessionId, ClientMessage),
}

pub struct EngineHandle {
    cmd_tx: SyncSender<EngineMessage>,
}

pub struct EngineSession {
    session_id: SessionId,
    cmd_tx: SyncSender<EngineMessage>,
}

pub fn start() -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::sync_channel(8);
    let (log_tx, _) = broadcast::channel(64);

    thread::spawn(move || {
        let mut engine = Engine {
            cmd_rx,
            log_tx,
            session_seq: Sequence::new(),
            modules: HashMap::new(),
            geometry: HashMap::new(),
            module_seq: Sequence::new(),
            connections: HashMap::new(),
            indications: HashMap::new(),
        };

        engine.run();
    });

    EngineHandle { cmd_tx }
}

#[derive(Debug)]
pub enum EngineError {
    Stopped,
    Busy,
}

impl<T> From<TrySendError<T>> for EngineError {
    fn from(e: TrySendError<T>) -> Self {
        match e {
            TrySendError::Full(_) => EngineError::Busy,
            TrySendError::Disconnected(_) => EngineError::Stopped,
        }
    }
}

pub type EngineOps = broadcast::Receiver<EngineOp>;

#[derive(Debug, Clone)]
pub enum EngineOp {
    Sync(OpClock),
    ServerUpdate(ServerUpdate),
}

impl EngineHandle {
    pub async fn connect(&self) -> Result<(WorkspaceState, EngineOps, EngineSession), EngineError> {
        let cmd_tx = self.cmd_tx.clone();

        let (tx, rx) = oneshot::channel();
        cmd_tx.try_send(EngineMessage::ConnectSession(tx))?;
        let (session_id, state, log_rx) = rx.await.map_err(|_| EngineError::Stopped)?;

        Ok((state, log_rx, EngineSession {
            cmd_tx,
            session_id,
        }))
    }
}

impl EngineSession {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// TODO - maybe pass log position in here and detect conflicts?
    pub fn update(&self, msg: ClientMessage) -> Result<(), EngineError> {
        self.send_message(EngineMessage::ClientMessage(self.session_id, msg))
    }

    fn send_message(&self, msg: EngineMessage) -> Result<(), EngineError> {
        Ok(self.cmd_tx.try_send(msg)?)
    }
}

pub struct Engine {
    cmd_rx: Receiver<EngineMessage>,
    log_tx: broadcast::Sender<EngineOp>,
    session_seq: Sequence,
    modules: HashMap<ModuleId, Module>,
    geometry: HashMap<ModuleId, WindowGeometry>,
    module_seq: Sequence,
    connections: HashMap<InputId, OutputId>,
    indications: HashMap<ModuleId, Indication>,
}

impl Engine {
    fn run(&mut self) {
        let start = Instant::now();
        let mut tick = 0;

        loop {
            let indications = self.run_tick(tick);
            tick += 1;

            for (module_id, indication) in indications {
                self.indications.insert(module_id, indication.clone());
                self.log_op(ServerUpdate::UpdateModuleIndication(module_id, indication));
            }

            let sleep_until = start + Duration::from_millis(tick * 1_000 / TICKS_PER_SECOND as u64);

            loop {
                let now = Instant::now();

                if now >= sleep_until {
                    break;
                }

                match self.cmd_rx.recv_timeout(sleep_until - now) {
                    Ok(EngineMessage::ConnectSession(tx)) => { let _ = tx.send(self.connect_session()); }
                    Ok(EngineMessage::ClientMessage(session, msg)) => { self.client_update(session, msg); }
                    Err(RecvTimeoutError::Timeout) => { break; }
                    Err(RecvTimeoutError::Disconnected) => { return; }
                }
            }
        }
    }

    fn connect_session(&mut self) -> (SessionId, WorkspaceState, EngineOps) {
        let session_id = SessionId(self.session_seq.next());
        let log_rx = self.log_tx.subscribe();
        let state = self.dump_state();
        (session_id, state, log_rx)
    }

    fn dump_state(&self) -> WorkspaceState {
        let mut state = WorkspaceState {
            modules: Vec::new(),
            geometry: Vec::new(),
            indications: Vec::new(),
            connections: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
        };

        for (module_id, module) in &self.modules {
            state.modules.push((*module_id, module.params()));
            state.inputs.push((*module_id, module.inputs().to_vec()));
            state.outputs.push((*module_id, module.outputs().to_vec()));
        }

        for (module_id, geometry) in &self.geometry {
            state.geometry.push((*module_id, geometry.clone()));
        }

        for (module_id, indication) in &self.indications {
            state.indications.push((*module_id, indication.clone()));
        }

        for (input, output) in &self.connections {
            state.connections.push((*input, *output));
        }

        state
    }

    fn log_op(&mut self, op: ServerUpdate) {
        let _ = self.log_tx.send(EngineOp::ServerUpdate(op));
    }

    fn sync_log(&mut self, clock: OpClock) {
        let _ = self.log_tx.send(EngineOp::Sync(clock));
    }

    fn client_update(&mut self, session_id: SessionId, msg: ClientMessage) {
        let clock = OpClock(session_id, msg.sequence);

        match msg.op {
            ClientOp::CreateModule(params, geometry) => {
                // TODO - the audio engine is not actually concerned with
                // window geometry and so should not own this data and force
                // all accesses to it to go via the live audio thread
                let id = ModuleId(self.module_seq.next());
                let (module, indication) = Module::create(params.clone());
                let inputs = module.inputs().to_vec();
                let outputs = module.outputs().to_vec();
                self.modules.insert(id, module);
                self.geometry.insert(id, geometry.clone());
                self.indications.insert(id, indication.clone());

                self.log_op(ServerUpdate::CreateModule {
                    id,
                    params,
                    geometry,
                    indication,
                    inputs,
                    outputs,
                });
            }
            ClientOp::UpdateModuleParams(module_id, params) => {
                if let Some(module) = self.modules.get_mut(&module_id) {
                    module.update(params.clone());
                    self.log_op(ServerUpdate::UpdateModuleParams(module_id, params));
                }
            }
            ClientOp::UpdateWindowGeometry(module_id, geometry) => {
                if let Some(geom) = self.geometry.get_mut(&module_id) {
                    *geom = geometry.clone();
                    self.log_op(ServerUpdate::UpdateWindowGeometry(module_id, geometry));
                }
            }
            ClientOp::DeleteModule(module_id) => {
                // find any connections connected to this module's inputs or
                // outputs and delete them, generating oplog entries

                let mut deleted_connections = Vec::new();

                for (input, output) in &self.connections {
                    if input.module_id() == module_id || output.module_id() == module_id {
                        deleted_connections.push(*input);
                    }
                }

                for deleted_connection in deleted_connections {
                    self.connections.remove(&deleted_connection);
                    self.log_op(ServerUpdate::DeleteConnection(deleted_connection));
                }

                // finally, delete the module:

                if self.modules.contains_key(&module_id) {
                    self.modules.remove(&module_id);
                    self.log_op(ServerUpdate::DeleteModule(module_id));
                }
            }
            ClientOp::CreateConnection(input_id, output_id) => {
                let input_type = match terminal_line_type(self, TerminalId::Input(input_id)) {
                    Some(ty) => ty,
                    None => return,
                };

                let output_type = match terminal_line_type(self, TerminalId::Output(output_id)) {
                    Some(ty) => ty,
                    None => return,
                };

                if input_type == output_type {
                    if let Some(_) = self.connections.insert(input_id, output_id) {
                        self.log_op(ServerUpdate::DeleteConnection(input_id));
                    }

                    self.log_op(ServerUpdate::CreateConnection(input_id, output_id));
                } else {
                    // type mismatch, don't connect
                }
            }
            ClientOp::DeleteConnection(input_id) => {
                if let Some(_) = self.connections.remove(&input_id) {
                    self.log_op(ServerUpdate::DeleteConnection(input_id));
                }
            }
        }

        return self.sync_log(clock);

        fn terminal_line_type(engine: &Engine, terminal: TerminalId) -> Option<LineType> {
            engine.modules.get(&terminal.module_id()).and_then(|module| {
                match terminal {
                    TerminalId::Input(input) => {
                        module.inputs().get(input.index()).map(|terminal| terminal.line_type())
                    }
                    TerminalId::Output(output) => {
                        module.outputs().get(output.index()).map(|terminal| terminal.line_type())
                    }
                }
            })
        }
    }

    fn run_tick(&mut self, tick: u64) -> Vec<(ModuleId, Indication)> {
        // find terminal modules - modules which do not send their output to
        // the input of any other module

        let mut terminal_modules = HashSet::new();

        for (id, _) in &self.modules {
            terminal_modules.insert(*id);
        }

        for (_, output) in &self.connections {
            terminal_modules.remove(&output.module_id());
        }

        // depth-first-search modules out via their inputs, starting from
        // terminal modules

        let mut topsort = Topsort {
            modules: &self.modules,
            connections: &self.connections,
            run_order: Vec::new(),
            seen: HashSet::new(),
        };

        for id in terminal_modules.into_iter() {
            traverse(id, &mut topsort);
        }

        struct Topsort<'a> {
            modules: &'a HashMap<ModuleId, Module>,
            connections: &'a HashMap<InputId, OutputId>,
            run_order: Vec<ModuleId>,
            seen: HashSet<ModuleId>,
        }

        fn traverse(module_id: ModuleId, state: &mut Topsort) {
            if state.seen.contains(&module_id) {
                return;
            }

            state.seen.insert(module_id);

            let module = &state.modules[&module_id];

            for i in 0..module.inputs().len() {
                let terminal_id = InputId(module_id, i);

                if let Some(output_id) = state.connections.get(&terminal_id) {
                    traverse(output_id.module_id(), state);
                }
            }

            state.run_order.push(module_id);
        }

        // run modules in dependency order according to BFS above

        let mut buffers = HashMap::<OutputId, Vec<Sample>>::new();
        let mut indications = Vec::new();

        for module_id in topsort.run_order.iter() {
            let module = self.modules.get_mut(&module_id)
                .expect("module get_mut");

            let connections = &self.connections;

            let mut output_buffers = Vec::<Vec<Sample>>::new();

            for _ in 0..module.outputs().len() {
                output_buffers.push(vec![0.0; SAMPLES_PER_TICK * CHANNELS]);
            }

            {
                let input_refs = module.inputs().iter()
                    .enumerate()
                    .map(|(i, ty)| (InputId(*module_id, i), ty))
                    .map(|(input, ty)|
                        connections.get(&input).map(|output|
                            &buffers[output][0..line_type_sample_count(ty.line_type())]))
                    .collect::<Vec<Option<&[Sample]>>>();

                let mut output_refs = output_buffers.iter_mut()
                    .zip(module.outputs())
                    .map(|(vec, ty)| &mut vec[0..line_type_sample_count(ty.line_type())])
                    .collect::<Vec<_>>();

                let t = tick * SAMPLES_PER_TICK as u64;

                match module.run_tick(t, &input_refs, &mut output_refs) {
                    None => {}
                    Some(indic) => {
                        indications.push((*module_id, indic));
                    }
                }
            }

            for (i, buffer) in output_buffers.into_iter().enumerate(){
                buffers.insert(OutputId(*module_id, i), buffer);
            }
        }

        indications
    }
}

fn line_type_sample_count(line_type: LineType) -> usize {
    match line_type {
        LineType::Mono => SAMPLES_PER_TICK,
        LineType::Stereo => SAMPLES_PER_TICK * 2,
    }
}

use std::fmt::{self, Debug};

use mixlab_protocol::{LineType, Terminal, ServerUpdate};

use crate::engine::{Sample, ZERO_BUFFER_STEREO};
use crate::module::ModuleT;

pub struct WebAudioDevice {
    params: (),
    inputs: Vec<Terminal>,
    outputs: Vec<Terminal>,
}

impl Debug for WebAudioDevice {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "WebAudioDevice {{ params: {:?}, .. }}", self.params)
    }
}

impl ModuleT for WebAudioDevice {
    type Params = ();
    type Indication = ();

    fn create(params: Self::Params) -> (Self, Self::Indication) {
        let indication = ();

        let device = WebAudioDevice {
            params,
            inputs: vec![LineType::Stereo.unlabeled()],
            outputs: vec![LineType::Stereo.unlabeled()],
        };

        (device, indication)
    }

    fn params(&self) -> Self::Params {
        self.params.clone()
    }

    fn update(&mut self, _new_params: Self::Params) -> Option<Self::Indication> {
        None
    }

    fn run_tick(&mut self, _t: u64, inputs: &[Option<&[Sample]>], outputs: &mut [&mut [Sample]]) -> Option<Self::Indication> {
        let input = inputs[0].unwrap_or(&ZERO_BUFFER_STEREO);
        let output = &mut outputs[0];
        output.clone_from_slice(input);

        None
    }

    fn inputs(&self) -> &[Terminal] {
        &self.inputs
    }

    fn outputs(&self)-> &[Terminal] {
        &self.outputs
    }
}
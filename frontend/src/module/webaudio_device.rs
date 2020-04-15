use std::fmt::{self, Display};
use std::iter;

use yew::{html, Component, ComponentLink, Html, ShouldRender, Properties};
use yew::components::Select;

use mixlab_protocol::{ModuleId, ModuleParams};

use crate::workspace::{Window, WindowMsg};

#[derive(Properties, Clone, Debug)]
pub struct WebAudioDeviceProps {
    pub id: ModuleId,
    pub module: ComponentLink<Window>,
    pub indication: (),
}

pub struct WebAudioDevice {
    props: WebAudioDeviceProps,
}

impl Component for WebAudioDevice {
    type Properties = WebAudioDeviceProps;
    type Message = ();

    fn create(props: Self::Properties, _: ComponentLink<Self>) -> Self {
        Self { props }
    }

    fn update(&mut self, _msg: Self::Message) -> ShouldRender {
        false
    }

    fn change(&mut self, props: Self::Properties) -> ShouldRender {
        self.props = props;
        true
    }

    fn view(&self) -> Html {
        html! {
            <>
            </>
        }
    }
}

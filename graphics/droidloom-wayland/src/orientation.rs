//! The display orientation each Android task asks for, told to Denial.

mod protocol {
    #![allow(
        dead_code,
        non_camel_case_types,
        non_upper_case_globals,
        non_snake_case,
        unused_imports,
        unused_unsafe,
        unused_variables,
        clippy::all,
        missing_docs
    )]
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/denial-orientation-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocol/denial-orientation-v1.xml");
}

pub(super) use protocol::denial_orientation_manager_v1::{
    DenialOrientationManagerV1, Orientation,
};
wayland_client::delegate_noop!(super::App: ignore DenialOrientationManagerV1);

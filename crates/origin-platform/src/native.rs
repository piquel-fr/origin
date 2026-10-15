//! The implementations of the platform traits, one module per supported
//! platform. Each calls its platform's own system calls, so only the one for
//! the build target is compiled.

cfg_select! {
    target_os = "linux" => {
        pub mod linux;
        /// The platform origin was built for.
        pub type Native = linux::Linux;
    }
    target_os = "macos" => {
        pub mod macos;
        /// The platform origin was built for.
        pub type Native = macos::MacOs;
    }
}

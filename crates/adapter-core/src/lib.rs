//! Platform-independent logic for the camera firmware, kept separate so it can
//! be unit-tested on the host: the interactive setup shell, the HTTP
//! request/response handling of the snapshot server and the UVC control
//! tables.
pub mod http;
pub mod shell;
pub mod uvc;

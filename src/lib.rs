//! Greenlight-rs: an xCloud / Xbox home streaming client built on
//! GStreamer's `webrtcbin`. A Rust port of
//! [Greenlight](https://github.com/unknownskl/greenlight).

pub mod api;
pub mod app;
pub mod auth;
pub mod channels;
pub mod config;
#[cfg(feature = "gui")]
pub mod gui;
pub mod http;
pub mod input;
pub mod player;
pub mod sdp;
pub mod session;
pub mod teredo;

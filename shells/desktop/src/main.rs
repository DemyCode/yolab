//! The desktop entry point. Everything real lives in the library — see lib.rs
//! for why: Android loads this crate as a cdylib rather than running a binary,
//! so the app cannot live in `main` and be reachable from both.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    yolab_desktop_lib::run()
}

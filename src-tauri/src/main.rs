#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod clipboard;
mod process;
mod recorder;
mod render;

fn main() {
    recorder::run();
}

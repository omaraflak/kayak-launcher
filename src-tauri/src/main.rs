// Release builds detach from the console so Windows does not open a terminal
// behind the launcher window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    kayak_launcher_lib::run()
}

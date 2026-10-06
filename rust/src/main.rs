//! Pony Town Anti-AFK - Rust port.
//! Reference implementation: the Python app in `../app/`.

fn main() -> iced::Result {
    pony_antiafk::app::App::run()
}

pub mod combined;
pub mod gamepad;

pub use combined::CombinedJoyConManager;
pub use gamepad::{VirtualGamepad, STICK_MAX, STICK_MIN, TRIGGER_MAX, TRIGGER_MIN};

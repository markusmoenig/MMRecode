//! Bit-oriented primitives shared by codec implementations.

mod reader;
mod start_code;
mod vlc;
mod writer;

pub use reader::BitReader;
pub use start_code::find_start_code_prefix;
pub use vlc::{VlcEntry, VlcTable};
pub use writer::BitWriter;

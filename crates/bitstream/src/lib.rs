//! Bit-oriented primitives shared by codec implementations.

mod reader;
mod start_code;
mod writer;

pub use reader::BitReader;
pub use start_code::find_start_code_prefix;
pub use writer::BitWriter;

use crate::{Preview, Request};
use std::io::Read;

pub(crate) fn render(req: &Request, size: u64) -> Preview {
    let mut file = match std::fs::File::open(&req.bytes_path) {
        Ok(file) => file,
        Err(error) => return Preview::Error(error.to_string()),
    };
    let mut head = Vec::with_capacity(4096);
    match file.by_ref().take(4096).read_to_end(&mut head) {
        Ok(_) => Preview::Hex { head, size },
        Err(error) => Preview::Error(error.to_string()),
    }
}

#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| keel_net::fuzz::wire(data));
